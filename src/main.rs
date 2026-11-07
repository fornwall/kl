//! Klethron Terminal Multiplexer.

fn main() {
    println!("Hello from Klethron Terminal Multiplexer!");
}

#[cfg(test)]
mod tests {
    #[test]
    fn unwrap_and_expect_are_allowed_in_tests() {
        let one: u8 = "1".parse().unwrap();
        let two: u8 = "2".parse().expect("2 is a valid u8");
        assert_eq!(one + two, 3);
    }
}
