//! Reached only through the harness, by an ordinary `[dependencies]` edge, and
//! named nothing in particular. The transitive case: the name rule this
//! replaced could not see it, because there is nothing about `sample-fixtures`
//! or about a normal dependency edge that says "test".

pub fn sample_json() -> String {
    let raw = std::fs::read_to_string("fixture.json").unwrap_or_default();
    raw
}
