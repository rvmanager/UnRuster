use std::collections::HashMap;

pub struct Store {
    items: Vec<u32>,
    by_name: HashMap<String, u32>,
}

impl Store {
    pub fn new() -> Self {
        Self { items: Vec::new(), by_name: HashMap::new() }
    }

    // Should fire api_leak: returns &Vec<...>
    pub fn items(&self) -> &Vec<u32> {
        &self.items
    }

    // Should fire api_leak: returns &HashMap<...>
    pub fn by_name(&self) -> &HashMap<String, u32> {
        &self.by_name
    }

    // Should fire api_leak: returns &mut Vec<...>
    pub fn items_mut(&mut self) -> &mut Vec<u32> {
        &mut self.items
    }

    // Should NOT fire — returns a slice, which is the recommended shape.
    pub fn items_slice(&self) -> &[u32] {
        &self.items
    }
}

// Should NOT fire — private fn, not on the public surface.
#[allow(dead_code)]
fn helper(_v: &Vec<u32>) {}
