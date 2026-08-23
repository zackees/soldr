// A hand-rolled "is it on" spelling set: exactly the shape soldr#2740 found
// five times, mutually disagreeing.

fn enabled(value: &str) -> bool {
    matches!(value.trim(), "1" | "true" | "yes" | "on")
}

fn main() {
    let _ = enabled("true");
}
