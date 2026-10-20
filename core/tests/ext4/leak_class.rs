//! The e2fsck leak-class classifier shared by the graded ext4 sweeps
//! (`ext4_writer_leak`, `ext4_rename`, `ext4_delete`). Included with
//! `#[path = "leak_class.rs"] mod leak_class;`, not a test target itself.
#![allow(dead_code)]

/// e2fsck lines that are leak-class: a block or inode the bitmaps claim
/// wrongly *in use* (a `-` difference: the bitmap says used, nothing owns it),
/// counters that disagree, and the published inode `ino` unattached. None of
/// them can show a user another file's data, cross-link two files, or corrupt
/// a directory. Returns every line of `text` that is NOT one of those.
///
/// Stricter than "mentions a bitmap": any `+` difference (something IS using a
/// block or inode the bitmap calls free, so the next allocation would
/// double-allocate it) is rejected, and an unattached/ref-count line is
/// accepted only for `inos`, the inodes the scenario publishes or replaces.
pub fn non_leak_lines(text: &str, inos: &[u64]) -> Vec<String> {
    const OK_PREFIX: [&str; 5] = [
        "Pass ",
        "e2fsck ",
        "Free blocks count wrong",
        "Free inodes count wrong",
        "Connect to /lost+found",
    ];
    let own = |l: &str, lead: &str| {
        l.strip_prefix(lead).is_some_and(|rest| {
            let num = rest.split(|c: char| !c.is_ascii_digit()).next().unwrap_or("");
            inos.iter().any(|i| num == i.to_string())
        })
    };
    text.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .filter(|l| !OK_PREFIX.iter().any(|p| l.starts_with(p)))
        .filter(|l| {
            let bitmap = l.starts_with("Block bitmap differences") || l.starts_with("Inode bitmap differences");
            !(bitmap && !l.contains('+'))
        })
        .filter(|l| {
            !(own(l, "Unattached inode ")
                || own(l, "Unattached zero-length inode ")
                || (l.contains(" ref count is") && own(l, "Inode ")))
        })
        .filter(|l| !l.starts_with("Fix? no"))
        .filter(|l| !l.contains("WARNING: Filesystem still has errors"))
        .filter(|l| !l.contains(" files (") && !l.contains("VERDICT"))
        // The "-n" answers and the ssh wrapper's own exit report, not findings.
        .filter(|l| *l != "IGNORED." && !(l.contains("level=fatal") && l.contains("exit status")))
        .map(String::from)
        .collect()
}
