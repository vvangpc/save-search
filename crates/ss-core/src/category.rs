//! 文件类型分类（按扩展名）。用于搜索结果的分类筛选与展示。

/// 搜索/展示用的文件类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    All,
    Folder,
    Document,
    Image,
    Video,
    Audio,
    Other,
}

const DOCUMENT: &[&str] = &[
    "txt", "md", "pdf", "doc", "docx", "rtf", "odt", "xls", "xlsx", "csv", "ppt", "pptx", "ods",
    "odp", "wps", "et", "dps",
];
const IMAGE: &[&str] = &[
    "jpg", "jpeg", "png", "gif", "bmp", "webp", "tif", "tiff", "svg", "ico", "heic", "raw", "psd",
];
const VIDEO: &[&str] = &[
    "mp4", "mkv", "avi", "mov", "wmv", "flv", "webm", "m4v", "mpg", "mpeg", "ts", "3gp", "rmvb",
];
const AUDIO: &[&str] = &["mp3", "wav", "flac", "aac", "ogg", "m4a", "wma", "opus", "aiff"];

/// 取文件名小写扩展名（不含点）；无扩展名返回空。
fn ext_lower(name: &[u8]) -> Vec<u8> {
    if let Some(pos) = name.iter().rposition(|&b| b == b'.') {
        if pos + 1 < name.len() {
            return name[pos + 1..].iter().map(|b| b.to_ascii_lowercase()).collect();
        }
    }
    Vec::new()
}

/// 对一个文件/文件夹归类（文件夹固定为 Folder）。
pub fn classify(name: &[u8], is_dir: bool) -> Category {
    if is_dir {
        return Category::Folder;
    }
    let ext = ext_lower(name);
    if ext.is_empty() {
        return Category::Other;
    }
    let e = ext.as_slice();
    let has = |set: &[&str]| set.iter().any(|s| s.as_bytes() == e);
    if has(DOCUMENT) {
        Category::Document
    } else if has(IMAGE) {
        Category::Image
    } else if has(VIDEO) {
        Category::Video
    } else if has(AUDIO) {
        Category::Audio
    } else {
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
