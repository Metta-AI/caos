fn main() {
    println!("mini says {}", double(21));
}

fn double(x: i32) -> i32 {
    x * 2
}

#[cfg(test)]
mod tests {
    #[test]
    fn doubles() {
        assert_eq!(super::double(21), 42);
    }
}
