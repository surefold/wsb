//! What a compiled module shows to the modules that use it.
//!
//! A module only has to be compiled again when something it can see changed.
//! So after compiling, this reads the `.class` files and hashes the part
//! another module can see: public and protected classes, fields and methods,
//! and the Kotlin metadata that carries what the JVM has no words for, such as
//! whether a type can be null. A change inside a function body leaves that hash
//! alone, and the rebuild stops.
//!
//! Getting this wrong in one direction only costs time, and in the other
//! produces a broken build, so anything unclear counts as a change. A file
//! that cannot be read as a class is hashed whole. The body of an `inline`
//! function is copied into its callers, so a module whose sources mention
//! `inline` has all of its code hashed. The metadata also lists private
//! members, so adding one counts as a change.

use std::fs;
use std::path::{Path, PathBuf};

/// A 128-bit FNV-1a hash. It is stable from one run to the next, which the
/// standard library's hasher does not promise. It is not cryptographic: it
/// notices changes, and is not a defence against someone crafting a collision.
#[derive(Debug, Clone)]
pub struct Fnv(u128);

impl Fnv {
    pub fn new() -> Fnv {
        Fnv(0x6c62272e07bb014262b821756295c58d)
    }

    pub fn update(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.0 ^= u128::from(b);
            self.0 = self.0.wrapping_mul(0x0000000001000000000000000000013B);
        }
    }

    pub fn finish(&self) -> String {
        format!("{:032x}", self.0)
    }
}

impl Default for Fnv {
    fn default() -> Self {
        Fnv::new()
    }
}

/// The hash of some bytes, as text.
pub fn hash(bytes: &[u8]) -> String {
    let mut h = Fnv::new();
    h.update(bytes);
    h.finish()
}

/// Every file under `dir`, sorted, so the order never depends on the disk.
pub fn files_under(dir: &Path) -> Result<Vec<PathBuf>, String> {
    fn visit(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), String> {
        let entries = fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        for entry in entries {
            let path = entry.map_err(|e| format!("{}: {e}", dir.display()))?.path();
            if path.is_dir() {
                visit(&path, out)?;
            } else {
                out.push(path);
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    visit(dir, &mut out)?;
    out.sort();
    Ok(out)
}

/// The hash of what the compiled classes under `dir` show to other modules.
/// With `include_code`, every byte counts (see the module documentation).
pub fn of_dir(dir: &Path, include_code: bool) -> Result<String, String> {
    let mut h = Fnv::new();
    for file in files_under(dir)? {
        let bytes = fs::read(&file).map_err(|e| format!("{}: {e}", file.display()))?;
        let name = file.strip_prefix(dir).unwrap_or(&file).to_string_lossy();
        let is_class = file.extension().is_some_and(|e| e == "class");
        let visible = if is_class && !include_code {
            class_api(&bytes)
        } else {
            None
        };
        match visible {
            // Not visible outside its package, so it cannot affect a user.
            Some(None) => {}
            Some(Some(text)) => {
                h.update(name.as_bytes());
                h.update(&text);
            }
            None => {
                h.update(name.as_bytes());
                h.update(&bytes);
            }
        }
    }
    Ok(h.finish())
}

const ACC_PUBLIC: u16 = 0x0001;
const ACC_PROTECTED: u16 = 0x0004;

struct Reader<'a> {
    bytes: &'a [u8],
    at: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Option<&'a [u8]> {
        let slice = self.bytes.get(self.at..self.at.checked_add(n)?)?;
        self.at += n;
        Some(slice)
    }
    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }
    fn u16(&mut self) -> Option<u16> {
        let b = self.take(2)?;
        Some(u16::from_be_bytes([b[0], b[1]]))
    }
    fn u32(&mut self) -> Option<u32> {
        let b = self.take(4)?;
        Some(u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
    }
}

/// One entry of a class file's constant pool, as far as this needs to know.
#[derive(Clone, Copy)]
enum Constant<'a> {
    Utf8(&'a [u8]),
    /// A class or string constant, which points at its text.
    Points(u16),
    Number(&'a [u8]),
    Other,
}

struct Pool<'a>(Vec<Constant<'a>>);

impl<'a> Pool<'a> {
    fn read(r: &mut Reader<'a>) -> Option<Pool<'a>> {
        let count = usize::from(r.u16()?);
        let mut entries = vec![Constant::Other; count.max(1)];
        let mut i = 1;
        while i < count {
            entries[i] = match r.u8()? {
                1 => {
                    let len = usize::from(r.u16()?);
                    Constant::Utf8(r.take(len)?)
                }
                3 | 4 => Constant::Number(r.take(4)?),
                5 | 6 => {
                    let number = Constant::Number(r.take(8)?);
                    entries[i] = number;
                    // Longs and doubles take up two slots.
                    i += 2;
                    continue;
                }
                7 | 8 | 16 | 19 | 20 => Constant::Points(r.u16()?),
                9..=12 | 17 | 18 => {
                    r.take(4)?;
                    Constant::Other
                }
                15 => {
                    r.take(3)?;
                    Constant::Other
                }
                _ => return None,
            };
            i += 1;
        }
        Some(Pool(entries))
    }

    fn get(&self, index: u16) -> Option<Constant<'a>> {
        self.0.get(usize::from(index)).copied()
    }

    /// The text of a constant: itself, or what it points at.
    fn text(&self, index: u16) -> Option<&'a [u8]> {
        match self.get(index)? {
            Constant::Utf8(text) => Some(text),
            Constant::Points(to) => match self.get(to)? {
                Constant::Utf8(text) => Some(text),
                _ => None,
            },
            _ => None,
        }
    }
}

fn push(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(bytes);
    out.push(0);
}

/// A class's visible part as bytes. `Some(None)` if nothing outside its
/// package can see it, and `None` if this is not a class file it understands.
fn class_api(bytes: &[u8]) -> Option<Option<Vec<u8>>> {
    let mut r = Reader { bytes, at: 0 };
    if r.u32()? != 0xCAFE_BABE {
        return None;
    }
    r.take(4)?; // the class file version
    let pool = Pool::read(&mut r)?;
    let access = r.u16()?;
    let this = r.u16()?;
    let parent = r.u16()?;
    if access & (ACC_PUBLIC | ACC_PROTECTED) == 0 {
        return Some(None);
    }
    let mut out = Vec::new();
    out.extend_from_slice(&access.to_be_bytes());
    push(&mut out, pool.text(this)?);
    if parent != 0 {
        push(&mut out, pool.text(parent)?);
    }
    for _ in 0..r.u16()? {
        let interface = r.u16()?;
        push(&mut out, pool.text(interface)?);
    }
    // Fields, then methods, each section marked so one cannot pass for the other.
    for section in [1, 2] {
        out.push(section);
        for _ in 0..r.u16()? {
            let access = r.u16()?;
            let name = r.u16()?;
            let descriptor = r.u16()?;
            let visible = access & (ACC_PUBLIC | ACC_PROTECTED) != 0;
            if visible {
                out.extend_from_slice(&access.to_be_bytes());
                push(&mut out, pool.text(name)?);
                push(&mut out, pool.text(descriptor)?);
            }
            for _ in 0..r.u16()? {
                let attribute = r.u16()?;
                let len = r.u32()? as usize;
                let info = r.take(len)?;
                // A constant's value is copied into the code that uses it.
                if visible && pool.text(attribute)? == b"ConstantValue" {
                    let index = u16::from_be_bytes([*info.first()?, *info.get(1)?]);
                    match pool.get(index)? {
                        Constant::Number(n) => push(&mut out, n),
                        _ => push(&mut out, pool.text(index)?),
                    }
                }
            }
        }
    }
    out.push(3);
    for _ in 0..r.u16()? {
        let attribute = r.u16()?;
        let len = r.u32()? as usize;
        let info = r.take(len)?;
        if pool.text(attribute)? == b"RuntimeVisibleAnnotations" {
            let mut a = Reader { bytes: info, at: 0 };
            for _ in 0..a.u16()? {
                annotation(&mut a, &pool, &mut out)?;
            }
        }
    }
    Some(Some(out))
}

/// Reads an annotation, keeping only the text of Kotlin's own metadata.
fn annotation(r: &mut Reader, pool: &Pool, out: &mut Vec<u8>) -> Option<()> {
    let is_metadata = pool.text(r.u16()?)? == b"Lkotlin/Metadata;";
    for _ in 0..r.u16()? {
        let name = pool.text(r.u16()?)?;
        element(
            r,
            pool,
            out,
            is_metadata && (name == b"d1" || name == b"d2"),
        )?;
    }
    Some(())
}

fn element(r: &mut Reader, pool: &Pool, out: &mut Vec<u8>, keep: bool) -> Option<()> {
    match r.u8()? {
        b's' => {
            let index = r.u16()?;
            if keep {
                push(out, pool.text(index)?);
            }
        }
        b'B' | b'C' | b'D' | b'F' | b'I' | b'J' | b'S' | b'Z' | b'c' => {
            r.take(2)?;
        }
        b'e' => {
            r.take(4)?;
        }
        b'@' => annotation(r, pool, out)?,
        b'[' => {
            for _ in 0..r.u16()? {
                element(r, pool, out, keep)?;
            }
        }
        _ => return None,
    }
    Some(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_hash_never_changes_from_one_run_to_the_next() {
        assert_eq!(hash(b""), "6c62272e07bb014262b821756295c58d");
        assert_ne!(hash(b"a"), hash(b"b"));
    }

    #[test]
    fn something_that_is_not_a_class_is_not_understood() {
        assert!(class_api(b"not a class").is_none());
        assert!(class_api(&[0xCA, 0xFE, 0xBA, 0xBE, 0, 0]).is_none());
    }
}
