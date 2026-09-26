//! 浏览器把 JSON 填进自己的模板，生成 HTML。
//! 模板两层：内置地板（编译期 include_str! 进二进制的出厂稿）+
//! 盘上层（/var/lib/aginxbrowser/templates，同名覆盖内置——scp 热修
//! 通道，改完即生效）。注册表也两层同序。调用方只交 JSON 和模板名。
//! 点开时才生成，所以很快。

use std::path::{Path, PathBuf};

pub const SHOW_PATH: &str = "/run/aginxbrowser/show.html";

/// /open 的失败形状。错误码是给母体的机器判据：`unknown_template` 带
/// known 清单，对应产品规程「没模板 → 报母体安排模型写一次并登记」；
/// 其余码是装机/环境问题（registry 缺、盘满），重试同一个调用没用。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpenError {
    /// templates 目录里没有 registry.json。
    NoRegistry(String),
    /// registry.json 在但解析不出 templates 数组。
    BadRegistry(String),
    /// 注册表里没有这个名字。known = 现在能用的全部模板名。
    UnknownTemplate { id: String, known: Vec<String> },
    /// 条目在册但 file 字段缺失，或模板文件读不出来。
    NoFile { id: String, why: String },
    /// show.html 落盘失败（/run 不在、盘满）。
    WriteShow(String),
}

impl OpenError {
    /// 稳定机器码，直接进 HTTP 响应的 error 字段。
    pub fn code(&self) -> &'static str {
        match self {
            OpenError::NoRegistry(_) => "no_registry",
            OpenError::BadRegistry(_) => "bad_registry",
            OpenError::UnknownTemplate { .. } => "unknown_template",
            OpenError::NoFile { .. } => "no_file",
            OpenError::WriteShow(_) => "write_show",
        }
    }
}

impl std::fmt::Display for OpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            OpenError::NoRegistry(dir) => write!(f, "no registry.json in {dir}"),
            OpenError::BadRegistry(why) => write!(f, "registry.json malformed: {why}"),
            OpenError::UnknownTemplate { id, known } => {
                write!(f, "unknown template {id}; known: {}", known.join(", "))
            }
            OpenError::NoFile { id, why } => write!(f, "template {id}: {why}"),
            OpenError::WriteShow(why) => write!(f, "write {SHOW_PATH}: {why}"),
        }
    }
}

pub fn dir() -> PathBuf {
    if let Ok(p) = std::env::var("AGINXBROWSER_TEMPLATES") {
        let p = PathBuf::from(p);
        if p.is_dir() {
            return p;
        }
    }
    let installed = PathBuf::from("/var/lib/aginxbrowser/templates");
    if installed.is_dir() {
        return installed;
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(parent) = exe.parent() {
            let beside = parent.join("templates");
            if beside.is_dir() {
                return beside;
            }
        }
    }
    installed
}

/// 内置模板地板：出厂模板编进二进制（id 顺序同 templates/registry.json）。
/// 盘上层（dir()）永远优先，内置只在注册表层垫底——盘上有同名条目则盘上
/// 的赢。这样裸装（fresh flash 无模板目录）也全套可用，而
/// /var/lib/aginxbrowser/templates 仍是 scp 秒生效的热修覆盖通道。
const BUILTIN: &[(&str, &str)] = &[
    ("weather", include_str!("../templates/weather.html")),
    ("reply", include_str!("../templates/reply.html")),
    ("qr", include_str!("../templates/qr.html")),
];

pub fn render(template: &str, data: &serde_json::Value) -> Result<String, OpenError> {
    let html = load(template)?;
    Ok(fill(&html, data))
}

/// 填模板并写成当前要显示的页。这个文件就是屏幕所有权：在 = 浏览器
/// 持屏，删掉 = 让位回开机画面（panel 盯着它）。
pub fn open(template: &str, data: &serde_json::Value) -> Result<usize, OpenError> {
    let html = render(template, data)?;
    if let Some(parent) = Path::new(SHOW_PATH).parent() {
        std::fs::create_dir_all(parent).map_err(|e| OpenError::WriteShow(e.to_string()))?;
    }
    let tmp = format!("{SHOW_PATH}.tmp");
    std::fs::write(&tmp, &html).map_err(|e| OpenError::WriteShow(e.to_string()))?;
    std::fs::rename(&tmp, SHOW_PATH).map_err(|e| OpenError::WriteShow(e.to_string()))?;
    Ok(html.len())
}

fn load(id: &str) -> Result<String, OpenError> {
    load_at(&dir(), id)
}
/// 解析注册表成 (id, file) 对。缺 id 或 file 的条目跳过——注册表是
/// 浏览器自己目录里的普通 JSON，写坏了不该炸掉整张清单。
fn registry_list_at(root: &Path) -> Result<Vec<(String, String)>, OpenError> {
    let raw = std::fs::read_to_string(root.join("registry.json"))
        .map_err(|_| OpenError::NoRegistry(root.display().to_string()))?;
    let doc: serde_json::Value =
        serde_json::from_str(&raw).map_err(|e| OpenError::BadRegistry(e.to_string()))?;
    let list = doc
        .get("templates")
        .and_then(|v| v.as_array())
        .ok_or_else(|| OpenError::BadRegistry("no templates array".to_string()))?;
    Ok(list
        .iter()
        .filter_map(|t| {
            let id = t.get("id")?.as_str()?;
            let file = t.get("file")?.as_str()?;
            Some((id.to_string(), file.to_string()))
        })
        .collect())
}

/// 盘上注册表 + 内置地板的合并查名。盘上条目按名赢内置；盘上完全没
/// 注册表（无目录/无文件 = 裸装形状）→ 只剩内置层；注册表在但解析不出
/// → 按坏热修报错，不静默吞；在册但文件读不出同理。
fn load_at(root: &Path, id: &str) -> Result<String, OpenError> {
    let disk = match registry_list_at(root) {
        Ok(list) => list,
        Err(e @ OpenError::BadRegistry(_)) => return Err(e),
        // registry_list_at 只产 NoRegistry/BadRegistry 两式；前者=裸装
        // 形状，落内置地板
        Err(_) => Vec::new(),
    };
    if let Some((_, file)) = disk.iter().find(|(k, _)| k == id) {
        return std::fs::read_to_string(root.join(file)).map_err(|e| OpenError::NoFile {
            id: id.to_string(),
            why: format!("{file}: {e}"),
        });
    }
    if let Some((_, html)) = BUILTIN.iter().find(|(k, _)| *k == id) {
        return Ok(html.to_string());
    }
    let mut names: Vec<&str> = Vec::new();
    for k in disk
        .iter()
        .map(|(k, _)| k.as_str())
        .chain(BUILTIN.iter().map(|(k, _)| *k))
    {
        if !names.contains(&k) {
            names.push(k);
        }
    }
    Err(OpenError::UnknownTemplate {
        id: id.to_string(),
        known: names.into_iter().map(str::to_string).collect(),
    })
}

fn fill(template: &str, data: &serde_json::Value) -> String {
    let mut out = template.to_string();
    if let Some(obj) = data.as_object() {
        for (k, v) in obj {
            let text = match v {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Number(n) => n.to_string(),
                serde_json::Value::Bool(b) => b.to_string(),
                serde_json::Value::Null => String::new(),
                other => other.to_string(),
            };
            out = out.replace(&format!("{{{{{k}}}}}"), &escape(&text));
        }
    }
    while let Some(start) = out.find("{{") {
        let Some(rel) = out[start + 2..].find("}}") else { break };
        let end = start + 2 + rel + 2;
        out.replace_range(start..end, "");
    }
    out
}

fn escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一个最小注册表目录，供 load_at 系测试用（不碰 env，可并行）。
    /// 目录名带单调计数：两个测试用了等长 registry，按 (pid, len) 命名会
    /// 共享目录，并行跑起来互相 remove_dir_all。
    fn fixture(registry: &str, files: &[(&str, &str)]) -> std::path::PathBuf {
        static FIXTURE_N: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);
        let n = FIXTURE_N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("aginxbrowser-tmpl-{}-{n}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("registry.json"), registry).unwrap();
        for (name, body) in files {
            std::fs::write(dir.join(name), body).unwrap();
        }
        dir
    }

    #[test]
    fn fills_slots_from_json() {
        let html = fill(
            "<p>{{city}}</p><p>{{temp}}</p>",
            &serde_json::json!({"city": "南京", "temp": 22}),
        );
        assert_eq!(html, "<p>南京</p><p>22</p>");
        assert!(!html.contains("{{"));
    }

    /// 没登记的模板名 → UnknownTemplate 且 known 带走全部可用名（盘上
    /// 优先去重，内置垫后）——这是 /open 返 404 的判据，母体拿清单走
    /// 「安排写模板」分支。
    #[test]
    fn unknown_template_carries_known_list() {
        let root = fixture(
            r#"{"templates":[{"id":"weather","file":"weather.html"},{"id":"listen","file":"listen.html"}]}"#,
            &[("weather.html", "w"), ("listen.html", "l")],
        );
        let err = load_at(&root, "晨报").unwrap_err();
        assert_eq!(err.code(), "unknown_template");
        match err {
            OpenError::UnknownTemplate { id, known } => {
                assert_eq!(id, "晨报");
                assert_eq!(known, vec!["weather", "listen", "reply", "qr"]);
            }
            other => panic!("expected UnknownTemplate, got {other:?}"),
        }
    }

    /// registry 坏 / 条目文件读不出各归各的码；registry 缺席（裸装形状）
    /// 不再报 NoRegistry——内置地板接住，只有连内置也没有的名字才 404。
    #[test]
    fn registry_and_file_failures_map_to_their_codes() {
        let empty = std::env::temp_dir().join(format!("aginxbrowser-tmpl-empty-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&empty);
        std::fs::create_dir_all(&empty).unwrap();
        assert!(matches!(
            load_at(&empty, "晨报"),
            Err(OpenError::UnknownTemplate { known, .. }) if known.contains(&"weather".to_string())
        ));

        let bad = fixture("{ not json", &[]);
        assert!(matches!(load_at(&bad, "x"), Err(OpenError::BadRegistry(_))));

        let noarr = fixture(r#"{"something":[]}"#, &[]);
        assert!(matches!(load_at(&noarr, "x"), Err(OpenError::BadRegistry(_))));

        let missing = fixture(
            r#"{"templates":[{"id":"weather","file":"weather.html"}]}"#,
            &[],
        );
        assert!(matches!(
            load_at(&missing, "weather"),
            Err(OpenError::NoFile { id, .. }) if id == "weather"
        ));

        // 在册在盘 → 读到模板原文
        let ok = fixture(
            r#"{"templates":[{"id":"weather","file":"weather.html"}]}"#,
            &[("weather.html", "<p>{{city}}</p>")],
        );
        assert_eq!(load_at(&ok, "weather").unwrap(), "<p>{{city}}</p>");
    }

    /// 内置地板：无任何盘上注册表时，出厂三模板原样可用（fresh flash
    /// 即全套——这正是内置层存在的理由）。
    #[test]
    fn builtin_floor_serves_without_disk_templates() {
        let bare = std::env::temp_dir().join(format!("aginxbrowser-tmpl-bare-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&bare);
        std::fs::create_dir_all(&bare).unwrap();
        for id in ["weather", "reply", "qr"] {
            let html = load_at(&bare, id).unwrap();
            assert!(html.starts_with('<'), "{id}: built-in should be HTML, got {} bytes", html.len());
        }
        // 内置清单防锈：三件齐、无漂流
        let ids: Vec<&str> = BUILTIN.iter().map(|(k, _)| *k).collect();
        assert_eq!(ids, vec!["weather", "reply", "qr"]);
    }

    /// 盘上同名赢内置：热修通道语义——scp 一份 weather.html 覆盖内置稿。
    #[test]
    fn disk_entry_overrides_builtin() {
        let hot = fixture(
            r#"{"templates":[{"id":"weather","file":"weather.html"}]}"#,
            &[("weather.html", "<p>HOTFIX</p>")],
        );
        assert_eq!(load_at(&hot, "weather").unwrap(), "<p>HOTFIX</p>");
        // 未覆盖的名字继续走内置
        assert!(load_at(&hot, "qr").unwrap().starts_with('<'));
    }

    /// render 全链：注册表 → 文件 → 填槽。fill 已有金测，这里钉 load+fill 接线。
    #[test]
    fn render_loads_and_fills() {
        let root = fixture(
            r#"{"templates":[{"id":"weather","file":"weather.html"}]}"#,
            &[("weather.html", "<p class=\"c\">{{city}} {{t}}°</p>")],
        );
        let html = render_at(&root, "weather", &serde_json::json!({"city": "南京", "t": 22})).unwrap();
        assert_eq!(html, "<p class=\"c\">南京 22°</p>");
    }

    fn render_at(root: &Path, id: &str, data: &serde_json::Value) -> Result<String, OpenError> {
        let html = load_at(root, id)?;
        Ok(fill(&html, data))
    }
}
