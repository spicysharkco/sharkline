//! What the OMC HUD showed about OMC and Claude Code themselves, read from
//! files OMC already keeps. Nothing here touches the network: the newer
//! versions come from `update-check.json`, which OMC's own update check
//! refreshes (the HUD only ever read it too).

use serde_json::Value;
use std::cmp::Ordering;
use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

/// `package.json` and the update cache are a few KiB.
const MAX_READ_BYTES: u64 = 256 * 1024;

/// `~/.claude`, or what `CLAUDE_CONFIG_DIR` says (`~` and `~/x` expand).
pub fn config_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from);
    resolve_config_dir(std::env::var("CLAUDE_CONFIG_DIR").ok().as_deref(), home)
}

fn resolve_config_dir(configured: Option<&str>, home: Option<PathBuf>) -> Option<PathBuf> {
    match configured.map(str::trim).filter(|c| !c.is_empty()) {
        None => Some(home?.join(".claude")),
        Some("~") => home,
        Some(c) => match c.strip_prefix("~/") {
            Some(rest) => Some(home?.join(rest)),
            None => Some(PathBuf::from(c)),
        },
    }
}

/// The installed OMC plugin the HUD ran from.
#[derive(Debug, PartialEq)]
pub struct Plugin {
    pub version: Option<String>,
    /// A development copy rather than a published package: it has a `.git` or
    /// a `src` directory next to `dist`. The HUD marked it with a trailing `L`.
    pub local: bool,
}

/// The plugin the HUD would have loaded: `OMC_PLUGIN_ROOT`, else the highest
/// version under the plugin cache that has a built HUD, else the marketplace
/// clone. `None` when OMC is not installed.
pub fn plugin(config: &Path) -> Option<Plugin> {
    let root = env_root()
        .or_else(|| cache_root(config))
        .or_else(|| marketplace_root(config))?;
    // Node imports the real path, and the checks below ran against that.
    let root = fs::canonicalize(&root).unwrap_or(root);
    Some(Plugin {
        version: package_version(&root),
        local: root.join(".git").exists() || root.join("src").exists(),
    })
}

fn has_hud(root: &Path) -> bool {
    root.join("dist/hud/index.js").exists()
}

fn env_root() -> Option<PathBuf> {
    let root = PathBuf::from(std::env::var("OMC_PLUGIN_ROOT").ok()?.trim());
    (!root.as_os_str().is_empty() && has_hud(&root)).then_some(root)
}

fn cache_root(config: &Path) -> Option<PathBuf> {
    let base = config.join("plugins/cache/omc/oh-my-claudecode");
    let mut versions: Vec<String> = fs::read_dir(&base)
        .ok()?
        .flatten()
        .filter_map(|e| e.file_name().into_string().ok())
        .filter(|name| is_version_dir(name))
        .collect();
    versions.sort_by(|a, b| compare_desc(a, b));
    versions
        .into_iter()
        .map(|v| base.join(v))
        .find(|dir| has_hud(dir))
}

fn marketplace_root(config: &Path) -> Option<PathBuf> {
    let root = config.join("plugins/marketplaces/omc");
    has_hud(&root).then_some(root)
}

/// `^\d+\.\d+\.\d+(-[A-Za-z0-9.-]+)?$`: strict, so a sibling such as
/// `4.14.1.backup` never outranks the real install.
fn is_version_dir(name: &str) -> bool {
    let (core, pre) = match name.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (name, None),
    };
    let mut numbers = core.split('.');
    (0..3).all(|_| {
        numbers
            .next()
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
    }) && numbers.next().is_none()
        && pre.is_none_or(|p| {
            !p.is_empty()
                && p.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
        })
}

/// Highest first; a release outranks a prerelease of the same number.
fn compare_desc(a: &str, b: &str) -> Ordering {
    fn split(v: &str) -> (&str, &str) {
        // The HUD's `split("-", 2)` keeps only the text up to the second dash.
        let mut parts = v.splitn(3, '-');
        (parts.next().unwrap_or(""), parts.next().unwrap_or(""))
    }
    let ((na, pa), (nb, pb)) = (split(a), split(b));
    let number = |n: &str| n.parse().unwrap_or(u64::MAX);
    nb.split('.')
        .map(number)
        .cmp(na.split('.').map(number))
        .then_with(|| match (pa.is_empty(), pb.is_empty()) {
            (true, true) => Ordering::Equal,
            (true, false) => Ordering::Less,
            (false, true) => Ordering::Greater,
            (false, false) => natural(pb, pa),
        })
}

/// Compare text with digit runs as numbers: `rc.10` after `rc.2`.
fn natural(mut a: &str, mut b: &str) -> Ordering {
    let split_digits = |s: &str| {
        let n = s.bytes().take_while(u8::is_ascii_digit).count();
        (s[..n].trim_start_matches('0').len(), n)
    };
    loop {
        match (a.chars().next(), b.chars().next()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(x), Some(y)) if x.is_ascii_digit() && y.is_ascii_digit() => {
                let ((sig_a, len_a), (sig_b, len_b)) = (split_digits(a), split_digits(b));
                let (run_a, run_b) = (
                    a[..len_a].trim_start_matches('0'),
                    b[..len_b].trim_start_matches('0'),
                );
                let ord = sig_a.cmp(&sig_b).then_with(|| run_a.cmp(run_b));
                if ord.is_ne() {
                    return ord;
                }
                (a, b) = (&a[len_a..], &b[len_b..]);
            }
            (Some(x), Some(y)) => {
                if x != y {
                    return x.cmp(&y);
                }
                (a, b) = (&a[x.len_utf8()..], &b[y.len_utf8()..]);
            }
        }
    }
}

/// `version` from the package's `package.json` (it must also have a `name`),
/// else the version in the directory name, else `None`.
fn package_version(root: &Path) -> Option<String> {
    let from_package = read_json(&root.join("package.json")).and_then(|pkg| {
        let text = |key: &str| {
            pkg.get(key)?
                .as_str()
                .filter(|s| !s.is_empty())
                .map(str::to_string)
        };
        text("name")?;
        text("version")
    });
    from_package.or_else(|| {
        let mut parts = root.components().map(|c| c.as_os_str().to_string_lossy());
        parts.find(|p| p == "oh-my-claudecode")?;
        let dir = parts.next()?;
        let numeric_start = dir.split('.').take(3).count() == 3
            && dir
                .split('.')
                .take(3)
                .all(|n| n.bytes().take_while(u8::is_ascii_digit).count() > 0);
        numeric_start.then(|| dir.into_owned())
    })
}

fn read_json(path: &Path) -> Option<Value> {
    if !fs::metadata(path).ok()?.is_file() {
        return None;
    }
    let mut buf = Vec::new();
    File::open(path)
        .ok()?
        .take(MAX_READ_BYTES)
        .read_to_end(&mut buf)
        .ok()?;
    serde_json::from_slice(&buf).ok()
}

/// The newer versions OMC's update check last saw.
#[derive(Debug, Default, PartialEq)]
pub struct Updates {
    pub omc: Option<String>,
    /// OMC was last checked against the plugin marketplace, not npm.
    pub marketplace: bool,
    pub claude: Option<String>,
}

/// `<config>/.omc/update-check.json`; every field is optional.
pub fn updates(config: &Path) -> Updates {
    let Some(doc) = read_json(&config.join(".omc/update-check.json")) else {
        return Updates::default();
    };
    let text = |key: &str| {
        doc.get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
    };
    Updates {
        omc: text("latestVersion"),
        marketplace: doc.get("source").and_then(Value::as_str) == Some("marketplace"),
        claude: text("claudeCodeLatestVersion"),
    }
}

/// True when `current` is older than `latest`. Dotted numbers, a leading `v`
/// ignored, anything that is not a number counting as 0.
pub fn is_older(current: &str, latest: &str) -> bool {
    let parts = |v: &str| -> Vec<u64> {
        v.strip_prefix('v')
            .unwrap_or(v)
            .split('.')
            .map(|n| {
                let digits = n.bytes().take_while(u8::is_ascii_digit).count();
                n[..digits].parse().unwrap_or(0)
            })
            .collect()
    };
    let (a, b) = (parts(current), parts(latest));
    (0..a.len().max(b.len()))
        .map(|i| a.get(i).unwrap_or(&0).cmp(b.get(i).unwrap_or(&0)))
        .find(|o| o.is_ne())
        == Some(Ordering::Less)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache::testing::Tmp;

    fn install(t: &Tmp, version: &str, built: bool, src: bool) {
        let root = format!("cfg/plugins/cache/omc/oh-my-claudecode/{version}");
        t.write(
            &format!("{root}/package.json"),
            &format!(r#"{{"name":"oh-my-claude-sisyphus","version":"{version}"}}"#),
        );
        if built {
            t.write(&format!("{root}/dist/hud/index.js"), "");
        }
        if src {
            t.write(&format!("{root}/src/index.ts"), "");
        }
    }

    #[test]
    fn picks_the_highest_built_version_and_flags_a_source_tree() {
        let t = Tmp::new("omc-pick");
        install(&t, "5.5.0", true, false);
        install(&t, "5.6.1", true, true);
        install(&t, "5.7.0", false, false); // not built: skipped
        let got = plugin(&t.join("cfg")).unwrap();
        assert_eq!(
            got,
            Plugin {
                version: Some("5.6.1".into()),
                local: true
            }
        );
    }

    #[test]
    fn published_package_is_not_local() {
        let t = Tmp::new("omc-published");
        install(&t, "5.6.1", true, false);
        let got = plugin(&t.join("cfg")).unwrap();
        assert_eq!(
            got,
            Plugin {
                version: Some("5.6.1".into()),
                local: false
            }
        );
        t.write("cfg/plugins/cache/omc/oh-my-claudecode/5.6.1/.git/HEAD", "");
        assert!(plugin(&t.join("cfg")).unwrap().local, ".git marks a clone");
    }

    #[test]
    fn symlinked_version_dirs_resolve_to_the_real_install() {
        let t = Tmp::new("omc-symlink");
        install(&t, "5.6.1", true, true);
        let base = t.join("cfg/plugins/cache/omc/oh-my-claudecode");
        std::os::unix::fs::symlink("5.6.1", base.join("9.9.9")).unwrap();
        // The highest name is the alias; the version read is the target's own.
        let got = plugin(&t.join("cfg")).unwrap();
        assert_eq!(got.version.as_deref(), Some("5.6.1"));
    }

    #[test]
    fn marketplace_clone_when_the_cache_is_empty() {
        let t = Tmp::new("omc-market");
        t.write("cfg/plugins/marketplaces/omc/dist/hud/index.js", "");
        t.write(
            "cfg/plugins/marketplaces/omc/package.json",
            r#"{"name":"x","version":"4.2.0"}"#,
        );
        assert_eq!(
            plugin(&t.join("cfg")),
            Some(Plugin {
                version: Some("4.2.0".into()),
                local: false
            })
        );
    }

    #[test]
    fn nothing_installed_is_none() {
        let t = Tmp::new("omc-none");
        std::fs::create_dir_all(t.join("cfg")).unwrap();
        assert_eq!(plugin(&t.join("cfg")), None);
        install(&t, "5.6.1", false, false);
        assert_eq!(plugin(&t.join("cfg")), None, "unbuilt");
    }

    #[test]
    fn version_falls_back_to_the_directory_name() {
        let t = Tmp::new("omc-nopkg");
        t.write(
            "cfg/plugins/cache/omc/oh-my-claudecode/4.11.2/dist/hud/index.js",
            "",
        );
        assert_eq!(
            plugin(&t.join("cfg")).unwrap().version.as_deref(),
            Some("4.11.2")
        );
        // package.json without a name is ignored, as in the HUD.
        t.write(
            "cfg/plugins/cache/omc/oh-my-claudecode/4.11.2/package.json",
            r#"{"version":"0.0.1"}"#,
        );
        assert_eq!(
            plugin(&t.join("cfg")).unwrap().version.as_deref(),
            Some("4.11.2")
        );
        // Not JSON at all.
        t.write(
            "cfg/plugins/cache/omc/oh-my-claudecode/4.11.2/package.json",
            "{",
        );
        assert_eq!(
            plugin(&t.join("cfg")).unwrap().version.as_deref(),
            Some("4.11.2")
        );
    }

    #[test]
    fn stray_sibling_directories_are_not_versions() {
        for ok in ["1.2.3", "10.0.0", "1.0.1-alpha", "1.0.0-rc.1", "1.0.0-rc-1"] {
            assert!(is_version_dir(ok), "{ok}");
        }
        for bad in [
            "1.2",
            "1.2.3.4",
            "1.2.3.backup",
            "_old",
            "1.2.x",
            "",
            "1.2.3-",
            "v1.2.3",
            "1.2.3-a b",
        ] {
            assert!(!is_version_dir(bad), "{bad}");
        }
    }

    #[test]
    fn versions_sort_highest_first_with_releases_above_prereleases() {
        let mut v = [
            "4.9.9",
            "5.0.0-rc.2",
            "5.0.0",
            "5.0.0-rc.10",
            "10.0.0",
            "5.0.0-alpha",
            "5.0.1",
        ];
        v.sort_by(|a, b| compare_desc(a, b));
        assert_eq!(
            v,
            [
                "10.0.0",
                "5.0.1",
                "5.0.0",
                "5.0.0-rc.10",
                "5.0.0-rc.2",
                "5.0.0-alpha",
                "4.9.9"
            ]
        );
    }

    #[test]
    fn version_order_keeps_numeric_bounds_digit_runs_and_stable_ties() {
        for (a, b, expected) in [
            ("01.02.003", "1.2.3", Ordering::Equal),
            (
                "18446744073709551616.0.0",
                "18446744073709551615.0.0",
                Ordering::Equal,
            ),
            ("1.2.3", "1.2.3-rc.1", Ordering::Less),
            ("1.2.3-rc.10", "1.2.3-rc.2", Ordering::Less),
            ("1.2.3-rc.02", "1.2.3-rc.2", Ordering::Equal),
            ("1.2.3-rc-99", "1.2.3-rc-1", Ordering::Equal),
            ("1.2.3--extra", "1.2.3", Ordering::Equal),
            (
                "1.2.3-rc.18446744073709551616",
                "1.2.3-rc.18446744073709551615",
                Ordering::Less,
            ),
        ] {
            assert!(is_version_dir(a), "{a}");
            assert!(is_version_dir(b), "{b}");
            assert_eq!(compare_desc(a, b), expected, "{a} versus {b}");
            assert_eq!(
                compare_desc(b, a),
                expected.reverse(),
                "reverse: {b} versus {a}"
            );
        }
        let mut versions = ["01.2.3", "1.02.3", "1.2.003", "1.2.3"];
        let before = versions;
        versions.sort_by(|a, b| compare_desc(a, b));
        assert_eq!(
            versions, before,
            "equal versions keep directory encounter order"
        );
    }

    #[test]
    fn update_check_is_read_when_present_and_ignored_when_not() {
        let t = Tmp::new("omc-updates");
        t.write(
            "cfg/.omc/update-check.json",
            r#"{"latestVersion":"5.7.0","source":"marketplace","claudeCodeLatestVersion":"2.1.291","extra":1}"#,
        );
        assert_eq!(
            updates(&t.join("cfg")),
            Updates {
                omc: Some("5.7.0".into()),
                marketplace: true,
                claude: Some("2.1.291".into())
            }
        );
        t.write(
            "cfg/.omc/update-check.json",
            r#"{"latestVersion":"5.7.0","source":"npm"}"#,
        );
        assert_eq!(
            updates(&t.join("cfg")),
            Updates {
                omc: Some("5.7.0".into()),
                marketplace: false,
                claude: None
            }
        );
        for bad in [
            "",
            "{",
            "[]",
            "null",
            r#"{"latestVersion":5,"claudeCodeLatestVersion":null}"#,
        ] {
            t.write("cfg/.omc/update-check.json", bad);
            assert_eq!(updates(&t.join("cfg")), Updates::default(), "{bad:?}");
        }
        assert_eq!(updates(&t.join("nowhere")), Updates::default());
    }

    #[test]
    fn version_comparison_follows_the_hud() {
        assert!(is_older("2.1.289", "2.1.291"));
        assert!(is_older("2.1.9", "2.1.10"));
        assert!(is_older("2.1", "2.1.1"));
        assert!(is_older("v1.0.0", "1.0.1"));
        assert!(!is_older("2.1.291", "2.1.291"));
        assert!(!is_older("2.2.0", "2.1.999"));
        assert!(!is_older("2.1.0", "2.1"));
        assert!(
            !is_older("5.6.1", "5.6.1-beta"),
            "a suffix reads as the number before it"
        );
        assert!(
            !is_older("x", "y"),
            "nothing numeric on either side is equal"
        );
    }

    #[test]
    fn config_dir_resolution() {
        let home = || Some(PathBuf::from("/home/u"));
        assert_eq!(
            resolve_config_dir(None, home()),
            Some("/home/u/.claude".into())
        );
        assert_eq!(
            resolve_config_dir(Some("  "), home()),
            Some("/home/u/.claude".into())
        );
        assert_eq!(resolve_config_dir(Some("~"), home()), home());
        assert_eq!(
            resolve_config_dir(Some("~/cfg"), home()),
            Some("/home/u/cfg".into())
        );
        assert_eq!(
            resolve_config_dir(Some(" /etc/claude "), home()),
            Some("/etc/claude".into())
        );
        assert_eq!(
            resolve_config_dir(Some("/etc/claude"), None),
            Some("/etc/claude".into())
        );
        assert_eq!(resolve_config_dir(None, None), None);
        assert_eq!(resolve_config_dir(Some("~/x"), None), None);
    }
}
