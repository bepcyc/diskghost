//! `diskghost agent`: runs on a remote host and answers line-delimited JSON
//! requests on stdin with JSON frames on stdout. M5 RED stub.

use std::io::{self, BufRead, Write};

/// Protocol version (`"v"`).
pub const PROTOCOL: u64 = 1;

/// Answer every request line of `input` on `out` until EOF.
pub fn serve<R: BufRead, W: Write>(input: R, out: W) -> io::Result<()> {
    let _ = (input, out);
    Ok(())
}
