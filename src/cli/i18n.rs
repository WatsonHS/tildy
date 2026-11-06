//! Human-facing UI language. English is the stable default; `auto` opts into
//! the user's message locale. Protocol fields and command names stay unchanged.

pub fn chinese() -> bool {
    chinese_with(&|key| std::env::var(key).ok())
}

fn chinese_with(env: &dyn Fn(&str) -> Option<String>) -> bool {
    let chosen = env("TILDY_LANG")
        .unwrap_or_else(|| "en".into())
        .to_ascii_lowercase();
    let locale = if chosen == "auto" {
        ["LC_ALL", "LC_MESSAGES", "LANG"]
            .into_iter()
            .find_map(|key| env(key).filter(|v| !v.is_empty()))
            .unwrap_or_default()
    } else {
        chosen
    }
    .to_ascii_lowercase();
    locale == "zh"
        || locale.starts_with("zh_")
        || locale.starts_with("zh-")
        || locale.starts_with("zh.")
}

pub fn text<'a>(en: &'a str, zh: &'a str) -> &'a str {
    if chinese() {
        zh
    } else {
        en
    }
}

/// Localize display descriptions; machine-readable metadata stays in English.
pub fn description(en: &str) -> &str {
    if !chinese() {
        return en;
    }
    match en {
        "Minimal single-line directory, Git branch, status and prompt" => "极简单行：目录、Git 分支与状态标记、❯",
        "p10k-style rainbow powerline with colored segments, status and time on the right; two rows" => "仿 p10k rainbow：每段一种底色的 powerline，第一行右侧状态、耗时与时间，第二行 ❯",
        "p10k-style classic powerline with neutral backgrounds, status and time on the right; two rows" => "仿 p10k classic：中性底色的 powerline，两侧细分隔，第一行右侧状态与时间，第二行 ❯",
        "p10k-style lean layout without backgrounds; directory and Git left, status and time right; two rows" => "仿 p10k lean：两行、无背景，左侧目录与 Git，第一行右侧状态、耗时与时间，第二行 ❯",
        "Palette-based powerline with user, directory, Git, languages and time; two rows" => "按角色着色的 powerline 两行：用户、目录、Git、语言、时间，第二行 ❯；用 palette 换色",
        "Starship-style layout using ASCII text symbols" => "Starship 默认外观，全部符号换成纯文字（固定 ascii 层）",
        "Starship-style layout showing language icons without versions" => "Starship 默认外观，只显示语言图标、不显示版本号",
        "Starship-style layout using Unicode symbols; no Nerd Font needed" => "Starship 默认外观，不用任何 Nerd 字体字形（固定 unicode 层）",
        "Starship-style layout; hides icons when tools are unavailable" => "Starship 默认外观；工具未安装时不显示空图标（tildy 本来就如此）",
        "Starship-style layout with Nerd Font icons for languages and tools" => "Starship 默认外观，语言与工具全部换成 Nerd 字体图标",
        "Starship-style modules in adjacent brackets" => "Starship 默认外观，各模块用方括号包起来紧挨排列",
        "Pure-style directory, muted Git branch, dirty marker and duration; purple prompt on a second row" => "仿 Pure：目录、灰色分支与 * 脏标记、命令耗时，第二行紫色 ❯",
        "Jetpack minimal left prompt, with directory, Git, languages and time on the right; no Nerd Font needed" => "Jetpack 极简左侧 ◎，目录、Git、语言、时间放在右侧（RPROMPT），几何符号无需 Nerd 字体",
        "Catppuccin Macchiato dark powerline; one row" => "Catppuccin Macchiato（深色）配色的同款 powerline 单行",
        "Catppuccin Frappe medium-dark powerline; one row" => "Catppuccin Frappé（中深色）配色的同款 powerline 单行",
        "Catppuccin Latte powerline for light terminals; one row" => "Catppuccin Latte（浅色背景用）配色的同款 powerline 单行",
        "Catppuccin Mocha dark powerline with duration and prompt on one row" => "Catppuccin Mocha（深色）彩色 powerline 单行，含命令耗时与 ❯",
        "Gruvbox rainbow with OS, user, directory, Git, languages, conda and time; two rows" => "Gruvbox 暖色彩虹 powerline：系统图标、用户、目录、Git、语言、conda、时间，第二行 Nerd 箭头",
        "Tokyo Night blue powerline with shaded start, Apple icon and rounded separators; two rows" => "Tokyo Night 深蓝渐变 powerline：░▒▓ 起头、苹果图标、圆角分隔，第二行 ❯",
        "Pastel powerline with user, directory, Git, languages and time; prompt on a second row" => "粉彩 powerline 两行布局，含用户、目录、Git、语言、时间，第二行 ❯",
        "Plain single-line layout without background colors; icons with Nerd Font" => "朴素外观：单行、无背景色，Nerd 字体层显示图标",
        "Catppuccin Mocha: deep pastel dark" => "Catppuccin Mocha：最深的粉彩深色",
        "Catppuccin Macchiato: dark" => "Catppuccin Macchiato：深色",
        "Catppuccin Frappe: medium-dark" => "Catppuccin Frappé：中深色",
        "Catppuccin Latte: for light terminals" => "Catppuccin Latte：浅色（浅色终端用）",
        "Tokyo Night: midnight blue with neon blue and purple" => "Tokyo Night（night）：深夜蓝底，霓虹蓝紫",
        "Gruvbox Dark: warm retro browns with orange, yellow and green" => "Gruvbox 深色：复古暖棕底，亮橙黄绿",
        "Kanagawa Wave: indigo with gold and blossom pink" => "Kanagawa（wave）：浮世绘靛蓝底，暗金与樱粉",
        "Everforest: muted forest green and warm accents" => "Everforest：森林绿灰底，暖色低饱和",
        "Rose Pine: dark purple with rose, gold and pine accents" => "Rosé Pine：暗紫底，玫瑰、金、松绿的柔和色",
        "One Dark: Atom-style gray with soft blue and purple" => "One Dark：Atom 默认深色，灰底配柔和蓝紫",
        "Solarized Light: cream backgrounds for light terminals" => "Solarized 浅色：米白底（浅色终端用）",
        "Solarized Dark: deep green with the classic low-contrast palette" => "Solarized 深色：墨绿底，经典低对比 16 色",
        "Nord: cool blue-gray with frost and aurora accents" => "Nord：北极冷色调，蓝灰底配霜蓝与极光色",
        "Dracula: dark purple-gray with vivid pink, purple, cyan and green" => "Dracula：深紫灰底，粉、紫、青、绿高饱和点缀",
        "Agent error" => "Agent 报错",
        "Agent waiting for approval (1 other record)" => "Agent 等待权限确认（另有 1 条记录）",
        "Detected Node, Rust and Python versions" => "node、rust、python 均检测到版本",
        "Two background jobs" => "两个后台任务",
        "Git and module data are stale (90 s)" => "Git 与模块数据过期（90 s）",
        "Git timed out with no value available" => "Git 数据超时，尚无值",
        "Narrow 40-column terminal with a deep path and dirty repository" => "40 列窄终端，深层目录与脏仓库",
        "SSH session (host build-01)" => "SSH 会话（主机 build-01）",
        "Previous command took 12.3 s" => "上一条命令耗时 12.3 s",
        "Previous command exited with status 1" => "上一条命令退出码 1",
        "3 commits ahead, 1 behind upstream" => "领先上游 3、落后 1",
        "Merge in progress with conflicts" => "合并中且有冲突文件",
        "Repository with unstaged, staged and untracked changes" => "有未暂存、已暂存与未跟踪改动的仓库",
        "Clean Git repository (main)" => "干净的 Git 仓库（main）",
        "Home directory ~" => "主目录 ~",
        "Ordinary directory outside a repository" => "普通目录，不在仓库内",
        _ => en,
    }
}

// Both translations are checked by Rust's formatter at compile time. Literal
// templates keep user/config values out of the formatting language itself.
macro_rules! tr {
    ($en:literal, $zh:literal $(, $arg:expr)* $(,)?) => {
        if $crate::cli::i18n::chinese() { format!($zh $(, $arg)*) } else { format!($en $(, $arg)*) }
    };
}
macro_rules! tr_println {
    ($($args:tt)*) => { println!("{}", $crate::cli::i18n::tr!($($args)*)) };
}
macro_rules! tr_eprintln {
    ($($args:tt)*) => { eprintln!("{}", $crate::cli::i18n::tr!($($args)*)) };
}
pub(crate) use {tr, tr_eprintln, tr_println};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn english_default_explicit_override_and_locale_precedence() {
        let check = |pairs: &[(&str, &str)]| {
            chinese_with(&|key| {
                pairs
                    .iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, v)| v.to_string())
            })
        };
        assert!(!check(&[]));
        assert!(!check(&[("LANG", "zh_CN.UTF-8")]));
        assert!(check(&[("TILDY_LANG", "zh-CN")]));
        assert!(!check(&[("TILDY_LANG", "en"), ("LANG", "zh_CN.UTF-8")]));
        assert!(!check(&[("TILDY_LANG", "fr")]));
        assert!(check(&[
            ("TILDY_LANG", "auto"),
            ("LC_ALL", ""),
            ("LC_MESSAGES", "zh_TW.UTF-8"),
            ("LANG", "en_US.UTF-8")
        ]));
        assert!(!check(&[
            ("TILDY_LANG", "auto"),
            ("LC_ALL", "C"),
            ("LC_MESSAGES", "zh_CN.UTF-8")
        ]));
    }
}
