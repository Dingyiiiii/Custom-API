//! Command customsd runs the Custom-API customs gateway.
//!
//! Placeholder entry point while the port is in progress; the real wiring lands
//! with the gateway module.
fn main() {
    println!("{}", customapi_core::version::identity());
}
