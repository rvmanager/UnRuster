//! Fixture exercising the `stringly_error` lint.

pub struct Service;

impl Service {
    // Should fire: `Result<_, String>` on a public method.
    pub fn parse(s: &str) -> Result<u32, String> {
        s.parse::<u32>().map_err(|e| e.to_string())
    }

    // Should fire: `Result<_, &str>` on a public method.
    pub fn lookup<'a>(_key: &str) -> Result<u32, &'a str> {
        Err("missing")
    }

    // Should NOT fire: private function.
    #[allow(dead_code)]
    fn helper() -> Result<(), String> {
        Ok(())
    }

    // Should NOT fire: structured error type.
    pub fn structured() -> Result<u32, MyError> {
        Err(MyError::Bad)
    }
}

// Should fire: free function with stringly error.
pub fn process(_input: &[u8]) -> Result<Vec<u8>, String> {
    Err("nope".to_string())
}

#[derive(Debug)]
pub enum MyError {
    Bad,
}

pub trait Backend {
    // Should fire: trait method with stringly error.
    fn fetch(&self, key: &str) -> Result<Vec<u8>, String>;
}
