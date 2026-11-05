//! 文件类型分类（按扩展名）。用于搜索结果的分类筛选与展示。
//! 全程不分配：分类在扫描热路径上对每个命中名字调用。

/// 搜索/展示用的文件类别。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Category {
    All,
    Folder,
    Document,
    Image,
    Media,
    Archive,
    App,
    Other,
}

impl Category {
    const COUNT: usize = 8;
}

/// 各分类的命中数。`get(Category::All)` 是不分分类的总数。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CategoryCounts([usize; Category::COUNT]);

impl CategoryCounts {
    pub fn get(&self, category: Category) -> usize {
        self.0[category as usize]
    }

    /// 记录一个属于 `category` 的命中（同时计入 `All`）。
    #[inline]
    pub(crate) fn add_hit(&mut self, category: Category) {
        self.0[Category::All as usize] += 1;
        self.0[category as usize] += 1;
    }

    pub(crate) fn merge(&mut self, other: &CategoryCounts) {
        for (a, b) in self.0.iter_mut().zip(other.0) {
            *a += b;
        }
    }
}

/// 最长的已知扩展名（`msixbundle`、`appxbundle`）不超过这个长度。
const MAX_KNOWN_EXT: usize = 10;

/// 取文件名扩展名切片（不含点、原大小写）；无扩展名返回空切片。
pub(crate) fn ext_of(name: &[u8]) -> &[u8] {
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
    if e.is_empty() || e.len() > MAX_KNOWN_EXT {
        return Category::Other;
    }
    let mut buf = [0u8; MAX_KNOWN_EXT];
    let lower = &mut buf[..e.len()];
    lower.copy_from_slice(e);
    lower.make_ascii_lowercase();
    match &*lower {
        b"txt" | b"md" | b"pdf" | b"doc" | b"docx" | b"rtf" | b"odt" | b"xls" | b"xlsx" | b"csv"
        | b"ppt" | b"pptx" | b"ods" | b"odp" | b"wps" | b"et" | b"dps" => Category::Document,
        b"jpg" | b"jpeg" | b"png" | b"gif" | b"bmp" | b"webp" | b"tif" | b"tiff" | b"svg"
        | b"ico" | b"heic" | b"raw" | b"psd" => Category::Image,
        b"mp3" | b"wav" | b"flac" | b"aac" | b"ogg" | b"oga" | b"m4a" | b"wma" | b"ape"
        | b"opus" | b"aiff" | b"mid" | b"midi" | b"mp4" | b"mkv" | b"avi" | b"mov" | b"wmv"
        | b"flv" | b"webm" | b"m4v" | b"mpg" | b"mpeg" | b"rm" | b"rmvb" | b"3gp" | b"vob"
        | b"m2ts" => Category::Media,
        b"zip" | b"rar" | b"7z" | b"tar" | b"gz" | b"bz2" | b"xz" | b"zst" | b"cab" | b"iso"
        | b"tgz" | b"tbz2" | b"lz" | b"lzma" | b"z" | b"arj" | b"ace" => Category::Archive,
        b"exe" | b"msi" | b"msix" | b"msixbundle" | b"appx" | b"appxbundle" | b"bat" | b"cmd"
        | b"ps1" | b"com" | b"scr" | b"lnk" | b"vbs" | b"jar" => Category::App,
        _ => Category::Other,
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
        assert_eq!(classify(b"a.mp4", false), Category::Media);
        assert_eq!(classify(b"a.FLAC", false), Category::Media);
        assert_eq!(classify(b"a.Exe", false), Category::App);
        assert_eq!(classify(b"a.msixbundle", false), Category::App);
        assert_eq!(classify(b"a.dll", false), Category::Other);
        assert_eq!(classify(b"a.averyveryverylongext", false), Category::Other);
        assert_eq!(classify(b"noext", false), Category::Other);
        assert_eq!(classify(b"trailing.", false), Category::Other);
        assert_eq!(classify(b"x.txt", true), Category::Folder);
    }

    #[test]
    fn counts_track_all_and_each_category() {
        let mut a = CategoryCounts::default();
        a.add_hit(Category::Media);
        a.add_hit(Category::App);
        let mut b = CategoryCounts::default();
        b.add_hit(Category::Media);
        a.merge(&b);
        assert_eq!(a.get(Category::All), 3);
        assert_eq!(a.get(Category::Media), 2);
        assert_eq!(a.get(Category::App), 1);
        assert_eq!(a.get(Category::Other), 0);
    }
}
