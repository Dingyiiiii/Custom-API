//! The vocabulary-free token estimator.
//!
//! The gateway is deliberately dependency-free and therefore cannot embed every
//! vendor BPE vocabulary. Instead it classifies text into tokenizer-relevant
//! segments (CJK, Latin words, digits, punctuation, whitespace, code) and
//! applies per-segment byte/character ratios calibrated against tiktoken
//! o200k/cl100k and the Anthropic tokenizer. This yields a bounded relative
//! error that is far smaller than the deviations a dishonest upstream would
//! need to produce to profit, which is what the check is for.

/// Identifies the tokenizer behaviour a model family uses.
///
/// Go declares this as `type TokenizerFamily string`, i.e. an open set: a value
/// read back from a ledger or supplied by configuration keeps its identity even
/// when it is not one of the constants below. The Rust port keeps the same
/// representation so that decoding an old record can never fail, and exposes the
/// known families as constants.
pub type TokenizerFamily = String;

/// Approximates the OpenAI cl100k_base tokenizer
/// (gpt-4, gpt-3.5, text-embedding-3-*).
pub const FAMILY_CL100K: &str = "cl100k";
/// Approximates the OpenAI o200k_base tokenizer
/// (gpt-4o, gpt-4.1, o-series).
pub const FAMILY_O200K: &str = "o200k";
/// Approximates the Anthropic Claude tokenizer.
pub const FAMILY_CLAUDE: &str = "claude";
/// Approximates the Google SentencePiece tokenizer.
pub const FAMILY_GEMINI: &str = "gemini";
/// The fallback for unknown models.
pub const FAMILY_GENERIC: &str = "generic";

/// Identifies families that encode a leading space as part of the token
/// (Gemini and other SentencePiece models).
///
/// Go exposes this as a method on the family type; the port keeps it as a free
/// function because [`TokenizerFamily`] is a plain string.
pub fn sentence_piece_delimiter(family: &str) -> bool {
    family == FAMILY_GEMINI
}

/// `cjkRatio` is the tokens-per-CJK-character ratio. CJK text is roughly one
/// token per character for the modern OpenAI/Anthropic vocabularies.
pub const CJK_RATIO: f64 = 1.0;

/// Maps a model identifier to a tokenizer family.
pub fn family_for(model: &str) -> TokenizerFamily {
    let m = model.to_lowercase();
    if m.contains("claude") {
        return FAMILY_CLAUDE.to_string();
    }
    if m.contains("gemini") || m.contains("gemma") || m.contains("palm") {
        return FAMILY_GEMINI.to_string();
    }
    if m.contains("o200k")
        || m.contains("gpt-4o")
        || m.contains("gpt-4.1")
        || m.contains("gpt-5")
        || m.contains("chatgpt-4o")
    {
        return FAMILY_O200K.to_string();
    }
    if m.contains("o1") || m.contains("o3") || m.contains("o4") {
        return FAMILY_O200K.to_string();
    }
    if m.contains("gpt-4")
        || m.contains("gpt-3.5")
        || m.contains("text-embedding")
        || m.contains("davinci")
    {
        return FAMILY_CL100K.to_string();
    }
    if m.contains("qwen")
        || m.contains("glm")
        || m.contains("deepseek")
        || m.contains("kimi")
        || m.contains("moonshot")
        || m.contains("yi-")
        || m.contains("baichuan")
        || m.contains("ernie")
        || m.contains("doubao")
        || m.contains("hunyuan")
        || m.contains("minimax")
        || m.contains("step-")
    {
        // Chinese vendors ship SentencePiece/BPE hybrids; the CJK-heavy
        // behaviour dominates and matches the generic ratios.
        return FAMILY_GENERIC.to_string();
    }
    FAMILY_GENERIC.to_string()
}

/// Holds tokens-per-byte for ASCII-ish scripts and an extra penalty for digit
/// runs, per family.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FamilyRatios {
    /// Tokens per byte for ordinary Latin text.
    pub ascii: f64,
    /// Tokens per byte for digit runs.
    pub digits: f64,
    /// Tokens per byte for punctuation/symbols.
    pub punct: f64,
    /// Tokens per newline (whitespace) run.
    pub newline: f64,
    /// Multiplier applied to [`CJK_RATIO`].
    pub cjk_scale: f64,
}

/// Returns the calibration table for one family.
///
/// These numbers are calibrated against real tokenizer output and are copied
/// verbatim from the Go implementation; changing one changes the measured error
/// characteristics (English +7..17%, Chinese +15%, JSON 0%, code -29%,
/// digit-dense -46%).
pub fn ratios_for(family: &str) -> FamilyRatios {
    match family {
        FAMILY_CL100K => FamilyRatios {
            ascii: 0.255,
            digits: 0.36,
            punct: 0.40,
            newline: 0.5,
            cjk_scale: 1.35,
        },
        FAMILY_O200K => FamilyRatios {
            ascii: 0.215,
            digits: 0.33,
            punct: 0.36,
            newline: 0.5,
            cjk_scale: 1.00,
        },
        FAMILY_CLAUDE => FamilyRatios {
            ascii: 0.220,
            digits: 0.34,
            punct: 0.38,
            newline: 0.5,
            cjk_scale: 1.15,
        },
        FAMILY_GEMINI => FamilyRatios {
            ascii: 0.240,
            digits: 0.35,
            punct: 0.39,
            newline: 0.5,
            cjk_scale: 1.05,
        },
        _ => FamilyRatios {
            ascii: 0.240,
            digits: 0.35,
            punct: 0.38,
            newline: 0.5,
            cjk_scale: 1.05,
        },
    }
}

/// Counts tokens for one tokenizer family.
#[derive(Clone, Debug, PartialEq)]
pub struct Estimator {
    family: TokenizerFamily,
    ratios: FamilyRatios,
}

impl Estimator {
    /// Returns an estimator for an explicit family.
    pub fn new(family: impl Into<TokenizerFamily>) -> Self {
        let family = family.into();
        Estimator {
            ratios: ratios_for(&family),
            family,
        }
    }

    /// Returns an estimator chosen from a model identifier.
    pub fn for_model(model: &str) -> Self {
        Estimator::new(family_for(model))
    }

    /// Returns the tokenizer family in use.
    pub fn family(&self) -> &TokenizerFamily {
        &self.family
    }

    /// Returns the calibration ratios in use.
    pub fn ratios(&self) -> FamilyRatios {
        self.ratios
    }

    /// Returns the estimated token count for a piece of text.
    ///
    /// The return value is always at least 1 for non-empty input, mirroring the
    /// behaviour of real tokenizers which emit at least one token for any
    /// content.
    pub fn estimate(&self, text: &str) -> i64 {
        if text.is_empty() {
            return 0;
        }
        let mut tokens = 0.0f64;
        let mut run_bytes: usize = 0;
        let mut current = classify_first(text);

        let flush = |class: TextClass, run_bytes: &mut usize, tokens: &mut f64| {
            if *run_bytes == 0 {
                return;
            }
            let n = *run_bytes as f64;
            match class {
                TextClass::Latin => *tokens += n * self.ratios.ascii,
                TextClass::Digits => *tokens += n * self.ratios.digits,
                TextClass::Punct => *tokens += n * self.ratios.punct,
                TextClass::Space => *tokens += n * self.ratios.newline,
                // 3 bytes per CJK char.
                TextClass::Cjk => *tokens += n * self.ratios.cjk_scale * CJK_RATIO / 3.0,
            }
            *run_bytes = 0;
        };

        for ch in text.chars() {
            let class = classify(ch);
            if class != current {
                flush(current, &mut run_bytes, &mut tokens);
                current = class;
            }
            run_bytes += ch.len_utf8();
        }
        flush(current, &mut run_bytes, &mut tokens);

        // Word-boundary correction.
        //
        // For long text the per-byte ratio is accurate because the average token
        // length converges. For short text it is not: real BPE vocabularies
        // contain whole common words (" instructions", " the"), so the true count
        // tracks the number of space-separated words more closely than the byte
        // count does. Blending the two removes a systematic bias that would
        // otherwise show up as a false usage-integrity signal on short prompts.
        let words = count_words(text);
        if words >= MIN_WORDS_FOR_CORRECTION {
            let word_estimate = words as f64 * words_per_token(&self.family);
            if word_estimate < tokens {
                tokens = 0.5 * tokens + 0.5 * word_estimate;
            }
        }

        // Go rounds with `int(tokens + 0.5)`, i.e. floor for non-negative input.
        let mut n = (tokens + 0.5) as i64;
        if n < 1 {
            n = 1;
        }
        n
    }

    /// Estimates tokens for raw bytes (used for streamed deltas).
    ///
    /// Go ranges over a string, so an invalid byte decodes as U+FFFD and is
    /// charged three bytes; the lossy conversion here reproduces that.
    pub fn estimate_bytes(&self, bytes: &[u8]) -> i64 {
        self.estimate(&String::from_utf8_lossy(bytes))
    }
}

/// `minWordsForCorrection` is the shortest text for which the word-boundary
/// blend is applied. Below it the estimate is dominated by a handful of tokens
/// and blending would add noise rather than remove bias.
pub const MIN_WORDS_FOR_CORRECTION: usize = 3;

/// `wordsPerToken` is the mean number of space-separated words encoded by one
/// token in ordinary prose, per family. It is the reciprocal of the average
/// token length in words.
pub fn words_per_token(family: &str) -> f64 {
    match family {
        FAMILY_O200K => 1.30,
        FAMILY_CL100K => 1.10,
        FAMILY_CLAUDE => 1.20,
        FAMILY_GEMINI => 1.15,
        _ => 1.15,
    }
}

/// Counts space-separated words, treating punctuation-only runs as non-words.
pub fn count_words(text: &str) -> usize {
    let mut n = 0usize;
    let mut in_word = false;
    for ch in text.chars() {
        if ch.is_whitespace() {
            in_word = false;
            continue;
        }
        if !in_word {
            n += 1;
            in_word = true;
        }
    }
    n
}

/// Labels a run of same-class characters.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TextClass {
    Latin,
    Cjk,
    Digits,
    Punct,
    Space,
}

fn classify(ch: char) -> TextClass {
    if matches!(ch, ' ' | '\t' | '\n' | '\r') {
        return TextClass::Space;
    }
    if is_cjk(ch) {
        return TextClass::Cjk;
    }
    if ch.is_ascii_digit() {
        return TextClass::Digits;
    }
    if ch.is_alphabetic() || is_mark(ch) {
        return TextClass::Latin;
    }
    TextClass::Punct
}

/// Reports whether a character belongs to one of the scripts Go's
/// `unicode.Han` / `Hiragana` / `Katakana` / `Hangul` tables cover.
///
/// Rust's standard library exposes no script property, so the ranges are spelled
/// out. They cover the assigned blocks of those scripts; adding a new Unicode
/// block only shifts the estimate for text that uses it.
fn is_cjk(ch: char) -> bool {
    let c = ch as u32;
    matches!(c,
        // Han: radicals and ideographs, including the supplementary planes.
        0x2E80..=0x2EFF
        | 0x2F00..=0x2FDF
        | 0x3005
        | 0x3007
        | 0x3021..=0x3029
        | 0x3038..=0x303B
        | 0x3400..=0x4DBF
        | 0x4E00..=0x9FFF
        | 0xF900..=0xFAFF
        | 0x20000..=0x3FFFF
        // Hiragana.
        | 0x3041..=0x3096
        | 0x309D..=0x309F
        // Katakana, including halfwidth and squared forms.
        | 0x30A1..=0x30FA
        | 0x30FD..=0x30FF
        | 0x31F0..=0x31FF
        | 0x32D0..=0x32FE
        | 0x3300..=0x3357
        | 0xFF66..=0xFF9D
        // Hangul: jamo, compatibility jamo and syllables.
        | 0x1100..=0x11FF
        | 0x3130..=0x318F
        | 0xA960..=0xA97F
        | 0xAC00..=0xD7A3
        | 0xD7B0..=0xD7FF
    )
}

/// Reports whether a character is a combining mark (Unicode `M*`).
///
/// Go calls `unicode.IsMark`. Rust has no general-category accessor without a
/// crate the workspace does not carry, so the combining blocks that occur in
/// practice are listed. A mark outside this table is classified as punctuation,
/// which changes the estimate for a run of marks by ~0.15 tokens per byte; no
/// calibration case in the test suite depends on it.
fn is_mark(ch: char) -> bool {
    let c = ch as u32;
    matches!(c,
        0x0300..=0x036F
        | 0x0483..=0x0489
        | 0x0591..=0x05BD
        | 0x05BF
        | 0x05C1..=0x05C2
        | 0x05C4..=0x05C5
        | 0x05C7
        | 0x0610..=0x061A
        | 0x064B..=0x065F
        | 0x0670
        | 0x06D6..=0x06DC
        | 0x06DF..=0x06E4
        | 0x06E7..=0x06E8
        | 0x06EA..=0x06ED
        | 0x0711
        | 0x0730..=0x074A
        | 0x07A6..=0x07B0
        | 0x07EB..=0x07F3
        | 0x0816..=0x082D
        | 0x0859..=0x085B
        | 0x08E3..=0x0903
        | 0x093A..=0x094F
        | 0x0951..=0x0957
        | 0x0962..=0x0963
        | 0x0981..=0x0983
        | 0x09BC
        | 0x09BE..=0x09CD
        | 0x0A01..=0x0A03
        | 0x0A3C..=0x0A51
        | 0x0A70..=0x0A71
        | 0x0A81..=0x0A83
        | 0x0ABC..=0x0ACD
        | 0x0B01..=0x0B03
        | 0x0B3C..=0x0B4D
        | 0x0B82..=0x0B83
        | 0x0BBE..=0x0BCD
        | 0x0C00..=0x0C03
        | 0x0C3E..=0x0C56
        | 0x0C81..=0x0C83
        | 0x0CBC..=0x0CD6
        | 0x0D01..=0x0D03
        | 0x0D3E..=0x0D4D
        | 0x0D82..=0x0D83
        | 0x0DCA..=0x0DDF
        | 0x0E31
        | 0x0E34..=0x0E3A
        | 0x0E47..=0x0E4E
        | 0x0EB1
        | 0x0EB4..=0x0EBC
        | 0x0EC8..=0x0ECD
        | 0x0F18..=0x0F19
        | 0x0F35
        | 0x0F37
        | 0x0F39
        | 0x0F3E..=0x0F3F
        | 0x0F71..=0x0F87
        | 0x102B..=0x103E
        | 0x1AB0..=0x1AFF
        | 0x1B00..=0x1B04
        | 0x1B34..=0x1B44
        | 0x1DC0..=0x1DFF
        | 0x20D0..=0x20F0
        | 0x2CEF..=0x2CF1
        | 0x2D7F
        | 0x302A..=0x302F
        | 0x3099..=0x309A
        | 0xA66F..=0xA672
        | 0xA674..=0xA67D
        | 0xA69E..=0xA69F
        | 0xFE00..=0xFE0F
        | 0xFE20..=0xFE2F
        | 0x101FD
        | 0x1D165..=0x1D169
        | 0x1D16D..=0x1D182
        | 0x1D185..=0x1D18B
        | 0x1D1AA..=0x1D1AD
        | 0x1E000..=0x1E02A
        | 0xE0100..=0xE01EF
    )
}

fn classify_first(text: &str) -> TextClass {
    match text.chars().next() {
        Some(ch) => classify(ch),
        // Go falls back to Latin when DecodeRuneInString reports RuneError; an
        // empty string never reaches the estimator, so this only documents the
        // degenerate case.
        None => TextClass::Latin,
    }
}
