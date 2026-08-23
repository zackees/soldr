use clap::Parser;
use regex::Regex;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};

#[derive(Debug, Parser)]
struct Args {
    #[arg(long, default_value_t = 2)]
    addend: i32,
}

#[derive(Debug, Serialize, Deserialize)]
struct Payload {
    value: i32,
}

extern "C" {
    fn fixture_add(left: i32, right: i32) -> i32;
}

fn main() {
    let args = Args::try_parse_from(["fixture", "--addend", "2"]).expect("parse args");
    let runtime = tokio::runtime::Runtime::new().expect("create runtime");
    runtime.block_on(async {
        let _client = reqwest::Client::builder().build().expect("build client");
    });
    assert!(Regex::new(r"^fixture$").expect("compile regex").is_match("fixture"));

    let connection = Connection::open_in_memory().expect("open sqlite");
    connection
        .execute("CREATE TABLE values_table (value INTEGER NOT NULL)", ())
        .expect("create table");
    let value = unsafe { fixture_add(40, args.addend) };
    connection
        .execute("INSERT INTO values_table (value) VALUES (?1)", [value])
        .expect("insert value");
    let payload = Payload { value };
    println!("{}", serde_json::to_string(&payload).expect("serialize payload"));
}
