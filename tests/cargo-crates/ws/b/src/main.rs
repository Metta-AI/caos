fn main() {
    println!("b says {}", a::double(21));
}

#[cfg(test)]
mod tests {
    #[test]
    fn doubles() {
        assert_eq!(a::double(21), 42);
    }
}
