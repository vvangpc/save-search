//! 文件类型分类（按扩展名）。用于搜索结果的分类筛选与展示。
//! 全程切片比较、不分配：分类过滤在扫描热路径上对每个命中名字调用。

/// 搜索/展示用的文件类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    All,
    Folder,
    Document,
    Image,
    Archive,
    Other,
}

const DOCUMENT: &[&str] = &[
    "txt", "md", "pdf", "doc", "docx", "rtf", "odt", "xls", "xlsx", "csv", "ppt", "pptx", "ods",
    "odp", "wps", "et", "dps",
];
const IMAGE: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "bmp", "webp", "tif", "tiff", "svg", "ico", "heic", "raw", "psd",
];
const ARCHIVE: &[&str] = &[
    "zip", "rar", "7z", "tar", "gz", "bz2", "xz", "zst", "cab", "iso", "tgz", "tbz2", "lz", "lzma",
    "z", "arj", "ace",
];

/// 取文件名扩展名切片（不含点、原大小写）；无扩展名返回空切片。
fn ext_of(name: &[u8]) -> &[u8] {
    match name.iter().rposition(|&b| b == b'.') {
        Some(pos) if pos + 1 < name.len() => &name[pos + 1..],
        _ => &[],
    }
}

/// 对一个文件/文件夹归类（文件夹固定为 Folder）。
pub fn classify(name: &[u8], is_dir: bool) -> Category {
    if is_dir {
        return Category::Folder;
    }
    let e = ext_of(name);
    if e.is_empty() {
        return Category::Other;
    }
    let has = |set: &[&str]| set.iter().any(|s| s.as_bytes().eq_ignore_ascii_case(e));
    if has(DOCUMENT) {
        Category::Document
    } else if has(IMAGE) {
        Category::Image
    } else if has(ARCHIVE) {
        Category::Archive
    } else {
        // 音频/视频及其他未归类 → Other
        Category::Other
    }
}

/// 节点是否匹配筛选类别。
pub fn matches(filter: Category, name: &[u8], is_dir: bool) -> bool {
    match filter {
        Category::All => true,
        Category::Folder => is_dir,
        other => !is_dir && classify(name, is_dir) == other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_by_extension_case_insensitive() {
        assert_eq!(classify(b"a.PDF", false), Category::Document);
        assert_eq!(classify(b"a.jpeg", false), Category::Image);
        assert_eq!(classify(b"a.7Z", false), Category::Archive);
        assert_eq!(classify(b"a.mp4", false), Category::Other);
        assert_eq!(classify(b"noext", false), Category::Other);
        assert_eq!(classify(b"trailing.", false), Category::Other);
        assert_eq!(classify(b"x.txt", true), Category::Folder);
    }
}
