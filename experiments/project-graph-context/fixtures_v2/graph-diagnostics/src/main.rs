mod diagnostics;
mod model;
mod resolve;

use std::io::{self, Read};

fn main() {
    let query = std::env::args().nth(1).unwrap_or_default();
    let mut input = String::new();
    io::stdin().read_to_string(&mut input).expect("stdin");
    match resolve::run_json(&input, &query) {
        Ok(value) => println!("{value}"),
        Err(error) => println!("{{\"ok\":false,\"code\":\"{error}\"}}"),
    }
}

