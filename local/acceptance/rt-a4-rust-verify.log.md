# A4 replace-body/diagnostics/safe-delete 复核 2026-09-21 01:58:47
### lib.rs BEFORE replace-body
```
pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

pub fn multiply(a: i32, b: i32) -> i32 {
    a * b
}```
null
### lib.rs AFTER replace-body multiply --with "    a * 2"
```
pub fn add(a: i32, b: i32) -> i32 {
    a + b
}

    a * 2```
### diagnostics in-tree fresh file (E0425 expected)
```
{
  "items": []
}
```
### rename lib.rs add→adder
```
{
  "edits_applied": 1,
  "files": [
    "lib.rs"
  ],
  "files_modified": 1
}
```
### lib.rs after rename
```
pub fn adder(a: i32, b: i32) -> i32 {
    a + b
}
```
DONE
