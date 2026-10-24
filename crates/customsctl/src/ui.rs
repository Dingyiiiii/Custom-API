//! A port of the parts of Go's `text/tabwriter` that the operator CLI uses.
//!
//! Every table `customsctl` prints goes through Go's tabwriter with the same
//! parameters: `tabwriter.NewWriter(os.Stdout, 0, 0, 2, ' ', 0)`. The deploy
//! scripts (`deploy/scripts/smoke-test.sh`, `verify-deployment.sh`) grep these
//! tables, and operators diff them against the Go build, so the spacing has to
//! be byte-identical rather than merely similar. Reimplementing the algorithm
//! is cheaper and far more reliable than hand-computing column widths in each
//! command.
//!
//! The relevant rules, from `text/tabwriter` with minwidth 0, padding 2,
//! padchar ' ', flags 0:
//!
//! - cells are separated by horizontal tabs; the text before the newline is the
//!   *trailing* cell and belongs to no column, so it is never padded;
//! - a column is as wide as its widest cell in the current block plus two
//!   spaces (the padding);
//! - a block ends at the first line that has no cell to the right of the
//!   current column, so a short line (such as `prices compare`'s `MODEL ...`
//!   banner) only aligns the columns it actually has;
//! - a line with a single cell is flushed immediately, which resets the block.
//!
//! Widths are counted in runes, exactly like Go's `utf8.RuneCount`, not in
//! display columns.

/// Two spaces of padding, matching `tabwriter.NewWriter(..., 2, ' ', 0)`.
const PADDING: usize = 2;

/// Minimum column width, matching the `minwidth` argument of 0.
const MIN_WIDTH: usize = 0;

/// One cell of a buffered line.
struct Cell {
    text: String,
    /// Width in runes, as Go computes it.
    width: usize,
}

/// One buffered line.
struct Row {
    cells: Vec<Cell>,
}

/// Buffers tab-separated lines and renders them column-aligned on [`finish`].
///
/// [`finish`]: TabWriter::finish
pub struct TabWriter {
    rows: Vec<Row>,
    out: String,
}

impl Default for TabWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl TabWriter {
    /// Creates an empty writer with the CLI's formatting parameters.
    pub fn new() -> Self {
        TabWriter {
            rows: Vec::new(),
            out: String::new(),
        }
    }

    /// Buffers one line. The line must not contain its terminating newline.
    ///
    /// A single-cell line is flushed on the spot, mirroring the tabwriter's
    /// "one cell cannot affect later formatting" shortcut; that is what keeps
    /// sections of unrelated output from being aligned with each other.
    pub fn line(&mut self, line: &str) {
        let cells: Vec<Cell> = line
            .split('\t')
            .map(|text| Cell {
                text: text.to_string(),
                width: text.chars().count(),
            })
            .collect();
        let single = cells.len() == 1;
        self.rows.push(Row { cells });
        if single {
            self.flush_section();
        }
    }

    /// Renders everything buffered so far and returns the text to print.
    pub fn finish(mut self) -> String {
        self.flush_section();
        self.out
    }

    /// Formats the buffered lines as one section.
    fn flush_section(&mut self) {
        if self.rows.is_empty() {
            return;
        }
        let mut rows = std::mem::take(&mut self.rows);
        // The tabwriter adds an empty line for the newline that terminated the
        // last row. That line prints nothing but supplies the final newline and
        // stops padding from being appended after the last real row.
        rows.push(Row { cells: Vec::new() });
        let total = rows.len();
        let mut widths: Vec<usize> = Vec::new();
        format_rows(&mut self.out, &rows, &mut widths, 0, total);
    }
}

/// Recursive formatter, a direct port of `(*tabwriter.Writer).format`.
///
/// `widths` holds the widths of the columns strictly to the left of the current
/// one; it is pushed and popped as the recursion descends, which is how Go
/// tracks the block boundaries per column.
fn format_rows(
    out: &mut String,
    rows: &[Row],
    widths: &mut Vec<usize>,
    line0: usize,
    line1: usize,
) {
    let column = widths.len();
    let mut line0 = line0;
    let mut this = line0;
    while this < line1 {
        // A line whose only cell at or after `column` is its trailing cell does
        // not participate in this column: cells are tab-terminated, so the last
        // cell of a line belongs to no column.
        if column >= rows[this].cells.len().saturating_sub(1) {
            this += 1;
            continue;
        }

        // Print the unprinted lines before the block.
        write_rows(out, rows, widths, line0, this);
        line0 = this;

        // Measure the column block starting here.
        let mut width = MIN_WIDTH;
        let mut end = this;
        while end < line1 {
            let line = &rows[end];
            if column >= line.cells.len().saturating_sub(1) {
                break;
            }
            let candidate = line.cells[column].width + PADDING;
            if candidate > width {
                width = candidate;
            }
            end += 1;
        }

        // Format and print every column to the right of this one, then the
        // lines themselves.
        widths.push(width);
        format_rows(out, rows, widths, line0, end);
        widths.pop();
        line0 = end;
        this = end;
    }

    write_rows(out, rows, widths, line0, line1);
}

/// Port of `(*tabwriter.Writer).writeLines`: prints cells, padding each one to
/// its column width (when the column has been measured) and left-aligning.
fn write_rows(out: &mut String, rows: &[Row], widths: &[usize], line0: usize, line1: usize) {
    for i in line0..line1 {
        let line = &rows[i];
        for (j, cell) in line.cells.iter().enumerate() {
            out.push_str(&cell.text);
            if j < widths.len() {
                for _ in cell.width..widths[j] {
                    out.push(' ');
                }
            }
        }
        // Go writes a newline after every line except the last buffered one; the
        // trailing empty row (added by `flush_section`) is that last line.
        if i + 1 != rows.len() {
            out.push('\n');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Renders rows the way a command does: push every line, then finish.
    fn render(lines: &[&str]) -> String {
        let mut tw = TabWriter::new();
        for line in lines {
            tw.line(line);
        }
        tw.finish()
    }

    #[test]
    fn columns_are_padded_to_widest_cell_plus_two() {
        // Expected text taken from the Go implementation's output for the same
        // rows, which is what the smoke test compares against.
        let out = render(&[
            "SEQ\tTIME\tACTOR\tCHANNEL\tMODEL\tDECISION\tTOKENS\tUPSTREAM\tVERDICT\tCOST",
            "1\t13:05:06\talice\talpha\tmock-basic\tallow\t200\t200\tconsistent\t0.003600",
            "2\t13:05:43\tbob\tbeta\tmock-basic\treport\t300\t360\tover_billed\t0.004100",
            "3\t13:06:20\talice\t(unrouted)\tgpt-x\tblock_user\t0\t0\tnot_reported\t0.000000",
        ]);
        let expected = "\
SEQ  TIME      ACTOR  CHANNEL     MODEL       DECISION    TOKENS  UPSTREAM  VERDICT       COST\n\
1    13:05:06  alice  alpha       mock-basic  allow       200     200       consistent    0.003600\n\
2    13:05:43  bob    beta        mock-basic  report      300     360       over_billed   0.004100\n\
3    13:06:20  alice  (unrouted)  gpt-x       block_user  0       0         not_reported  0.000000\n";
        assert_eq!(out, expected);
    }

    #[test]
    fn a_short_banner_line_aligns_only_its_own_columns() {
        // `prices compare` prints a three-cell banner above a nine-cell table.
        // Go aligns the banner against the table columns it shares, and leaves
        // the banner's trailing cell unpadded.
        let out = render(&[
            "MODEL\tmock-basic\t(1000 in / 500 out tokens)",
            "CHANNEL\tTRUST\tIN/M\tOUT/M\tCUR\tLIST\tCOST\tDISC%\tPROMOTIONS",
            "beta\tuntrusted\t8.0000\t24.0000\tUSD\t0.020000\t0.020000\t0.00\t",
            "alpha\ttrusted\t10.0000\t30.0000\tCNY\t0.025000\t0.025000\t0.00\t",
        ]);
        let expected = "\
MODEL    mock-basic  (1000 in / 500 out tokens)\n\
CHANNEL  TRUST       IN/M     OUT/M    CUR  LIST      COST      DISC%  PROMOTIONS\n\
beta     untrusted   8.0000   24.0000  USD  0.020000  0.020000  0.00   \n\
alpha    trusted     10.0000  30.0000  CNY  0.025000  0.025000  0.00   \n";
        assert_eq!(out, expected);
    }

    #[test]
    fn a_single_cell_line_breaks_the_column_block() {
        // Go flushes after a one-cell line, so the width of the second table is
        // not inflated by the first one.
        let out = render(&["a\tb", "long-value\tb", "header", "x\ty", "zzz\ty"]);
        let expected = "\
a           b\n\
long-value  b\n\
header\n\
x    y\n\
zzz  y\n";
        assert_eq!(out, expected);
    }

    #[test]
    fn empty_trailing_cell_keeps_the_padding_of_the_previous_column() {
        let out = render(&["DISC%\tPROMOTIONS", "0.00\t", "10.00\tvolume×0.900"]);
        let expected = "\
DISC%  PROMOTIONS\n\
0.00   \n\
10.00  volume×0.900\n";
        assert_eq!(out, expected);
    }

    #[test]
    fn widths_are_counted_in_runes() {
        // Multi-byte text must be measured by characters, not bytes, or the
        // promotion column would drift in a CJK model name.
        let out = render(&["NAME\tVALUE", "模型\t1", "ab\t2"]);
        let expected = "NAME  VALUE\n模型    1\nab    2\n";
        assert_eq!(out, expected);
    }

    #[test]
    fn user_table_matches_the_go_rendering() {
        let out = render(&[
            "USER\tTRUST\tPERIOD\tQUOTA\tCONSUMED\tREMAINING\tREQUESTS\tSPEND\tLAST SEEN",
            "alice\ttrusted\tmonthly\t0\t0\tunlimited\t5\t0.123400\t03-04 13:05",
            "bob\tuntrusted\tdaily\t500000\t1200\t498800\t2\t0.000000\t-",
        ]);
        let expected = "\
USER   TRUST      PERIOD   QUOTA   CONSUMED  REMAINING  REQUESTS  SPEND     LAST SEEN\n\
alice  trusted    monthly  0       0         unlimited  5         0.123400  03-04 13:05\n\
bob    untrusted  daily    500000  1200      498800     2         0.000000  -\n";
        assert_eq!(out, expected);
    }

    #[test]
    fn finish_without_lines_is_empty() {
        assert_eq!(TabWriter::new().finish(), "");
    }
}
