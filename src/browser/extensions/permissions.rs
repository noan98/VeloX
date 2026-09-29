//! 拡張機能ごとの承認済み権限 (Issue #83 / #84、docs/extensions.md §4)。
//!
//! 「宣言 (マニフェスト) は要求であって付与ではない」。**API 呼び出しの可否は
//! ここの `Approval` だけで決める** (マニフェストを直接見ない)。承認は
//! - インストール/再承認時にユーザが承認した必須の権限・ホスト
//! - 実行時に承認された任意 (`optional_*`) の権限・ホスト
//!
//! の 2 組で、文字列 (権限名・ホストパターンの正規形) として永続する。
//! 読み込み時は必ず `sanitize` でマニフェストの宣言に切り詰める (改ざんされた
//! 承認記録で宣言外の権限を得られないように)。

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::browser::extension_manifest::{HostPattern, Manifest, Permission};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct Approval {
    /// 承認済みの必須権限 (`Permission::as_str`)。
    pub permissions: BTreeSet<String>,
    /// 承認済みの必須ホスト (`HostPattern` の正規形)。`content_scripts.matches` を含む。
    pub hosts: BTreeSet<String>,
    /// 実行時に付与された任意権限。
    pub optional_permissions: BTreeSet<String>,
    /// 実行時に付与された任意ホスト。
    pub optional_hosts: BTreeSet<String>,
}

fn required_permissions(m: &Manifest) -> BTreeSet<String> {
    m.permissions
        .iter()
        .map(|p| p.as_str().to_owned())
        .collect()
}

fn required_hosts(m: &Manifest) -> BTreeSet<String> {
    m.required_host_patterns()
        .into_iter()
        .map(|p| p.to_string())
        .collect()
}

fn optional_permission_names(m: &Manifest) -> BTreeSet<String> {
    m.optional_permissions
        .iter()
        .map(|p| p.as_str().to_owned())
        .collect()
}

fn optional_host_names(m: &Manifest) -> BTreeSet<String> {
    m.optional_host_permissions
        .iter()
        .map(|p| p.to_string())
        .collect()
}

impl Approval {
    /// マニフェストの必須宣言をすべて承認した状態 (インストール/再承認)。
    /// 任意権限は付与しない。
    pub fn for_manifest(m: &Manifest) -> Approval {
        Approval {
            permissions: required_permissions(m),
            hosts: required_hosts(m),
            ..Approval::default()
        }
    }

    /// マニフェストの必須宣言のうち、まだ承認されていないもの (権限, ホスト)。
    /// 空でなければ「承認を超えている」= 再承認が必要。
    pub fn missing_for(&self, m: &Manifest) -> (Vec<String>, Vec<String>) {
        let perms = required_permissions(m)
            .difference(&self.permissions)
            .cloned()
            .collect();
        let hosts = required_hosts(m).difference(&self.hosts).cloned().collect();
        (perms, hosts)
    }

    pub fn exceeded_by(&self, m: &Manifest) -> bool {
        let (p, h) = self.missing_for(m);
        !p.is_empty() || !h.is_empty()
    }

    /// 承認をマニフェストの宣言の範囲に切り詰める (宣言から消えた権限は失効し、
    /// 宣言外の名前は捨てる)。
    pub fn sanitize(&mut self, m: &Manifest) {
        let req_p = required_permissions(m);
        let req_h = required_hosts(m);
        let opt_p = optional_permission_names(m);
        let opt_h = optional_host_names(m);
        self.permissions.retain(|p| req_p.contains(p));
        self.hosts.retain(|h| req_h.contains(h));
        self.optional_permissions.retain(|p| opt_p.contains(p));
        self.optional_hosts.retain(|h| opt_h.contains(h));
    }

    pub fn has_permission(&self, p: Permission) -> bool {
        self.permissions.contains(p.as_str()) || self.optional_permissions.contains(p.as_str())
    }

    /// `url` が承認済みホストのどれかに一致するか (`http` / `https` のみ)。
    pub fn allows_url(&self, url: &str) -> bool {
        self.hosts
            .iter()
            .chain(self.optional_hosts.iter())
            .filter_map(|s| HostPattern::parse(s).ok())
            .any(|p| p.matches(url))
    }

    /// 任意権限・任意ホストを、マニフェストが宣言しているものに限って付与する。
    /// 宣言外があれば何も変更せず `false`。
    pub fn grant_optional(
        &mut self,
        m: &Manifest,
        permissions: &[Permission],
        hosts: &[HostPattern],
    ) -> bool {
        let decl_p = optional_permission_names(m);
        let decl_h = optional_host_names(m);
        if permissions.iter().any(|p| !decl_p.contains(p.as_str()))
            || hosts.iter().any(|h| !decl_h.contains(&h.to_string()))
        {
            return false;
        }
        self.optional_permissions
            .extend(permissions.iter().map(|p| p.as_str().to_owned()));
        self.optional_hosts
            .extend(hosts.iter().map(|h| h.to_string()));
        true
    }

    /// 付与済みの任意権限・任意ホストを取り消す。必須の承認には触れない。
    /// 何か取り消したら `true`。
    pub fn revoke_optional(&mut self, permissions: &[Permission], hosts: &[HostPattern]) -> bool {
        let mut changed = false;
        for p in permissions {
            changed |= self.optional_permissions.remove(p.as_str());
        }
        for h in hosts {
            changed |= self.optional_hosts.remove(&h.to_string());
        }
        changed
    }

    /// 要求された任意権限・ホストがすべて付与済みか。
    pub fn contains_all(&self, permissions: &[Permission], hosts: &[HostPattern]) -> bool {
        permissions.iter().all(|p| self.has_permission(*p))
            && hosts.iter().all(|h| {
                let s = h.to_string();
                self.hosts.contains(&s) || self.optional_hosts.contains(&s)
            })
    }

    /// 承認の記録 (画面・ログ用) 。
    pub fn permission_names(&self) -> Vec<String> {
        self.permissions
            .union(&self.optional_permissions)
            .cloned()
            .collect()
    }

    pub fn host_names(&self) -> Vec<String> {
        self.hosts.union(&self.optional_hosts).cloned().collect()
    }

    /// 更新時に持ち越す任意付与 (新マニフェストにも任意宣言があるものだけ)。
    pub fn carry_over_optional(&self, m: &Manifest) -> Approval {
        let mut a = Approval {
            optional_permissions: self.optional_permissions.clone(),
            optional_hosts: self.optional_hosts.clone(),
            ..Approval::default()
        };
        let opt_p = optional_permission_names(m);
        let opt_h = optional_host_names(m);
        a.optional_permissions.retain(|p| opt_p.contains(p));
        a.optional_hosts.retain(|h| opt_h.contains(h));
        a
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::browser::extension_manifest::parse_manifest;

    fn manifest(extra: &str) -> Manifest {
        let json = format!(
            r#"{{"manifest_version":1,"id":"com.example.p","name":"P","version":"1.0.0"{extra}}}"#
        );
        parse_manifest(json.as_bytes()).expect("valid manifest")
    }

    #[test]
    fn install_approval_covers_required_only() {
        let m = manifest(
            r#","permissions":["storage"],"optional_permissions":["tabs"],
            "host_permissions":["https://a.example.com/*"],
            "content_scripts":[{"matches":["https://b.example.com/*"],"js":["c.js"]}]"#,
        );
        let a = Approval::for_manifest(&m);
        assert!(a.has_permission(Permission::Storage));
        assert!(!a.has_permission(Permission::Tabs));
        assert!(a.allows_url("https://a.example.com/x"));
        // content_scripts.matches はホストアクセスとして承認に含まれる。
        assert!(a.allows_url("https://b.example.com/"));
        assert!(!a.allows_url("https://c.example.com/"));
        assert!(!a.exceeded_by(&m));
    }

    #[test]
    fn optional_grant_is_limited_to_declared_items() {
        let m = manifest(
            r#","optional_permissions":["tabs"],"optional_host_permissions":["https://o.example.org/*"]"#,
        );
        let mut a = Approval::for_manifest(&m);
        let o = HostPattern::parse("https://o.example.org/*").unwrap();
        assert!(!a.grant_optional(&m, &[Permission::Scripting], &[]));
        assert!(!a.grant_optional(
            &m,
            &[],
            &[HostPattern::parse("https://x.example.org/*").unwrap()]
        ));
        assert!(a.optional_permissions.is_empty());
        assert!(a.grant_optional(&m, &[Permission::Tabs], std::slice::from_ref(&o)));
        assert!(a.has_permission(Permission::Tabs));
        assert!(a.allows_url("https://o.example.org/a"));
        assert!(a.revoke_optional(&[Permission::Tabs], &[o]));
        assert!(!a.has_permission(Permission::Tabs));
        assert!(!a.allows_url("https://o.example.org/a"));
    }

    #[test]
    fn exceeded_by_detects_new_permissions_and_hosts() {
        let old = manifest(r#","permissions":["storage"]"#);
        let a = Approval::for_manifest(&old);
        let newer = manifest(
            r#","permissions":["storage","tabs"],"host_permissions":["https://n.example.com/*"]"#,
        );
        let (p, h) = a.missing_for(&newer);
        assert_eq!(p, vec!["tabs".to_owned()]);
        assert_eq!(h, vec!["https://n.example.com/*".to_owned()]);
        assert!(a.exceeded_by(&newer));
        // 減る更新は超えない。
        let fewer = manifest("");
        assert!(!a.exceeded_by(&fewer));
    }

    #[test]
    fn sanitize_drops_anything_the_manifest_does_not_declare() {
        let m = manifest(r#","permissions":["storage"]"#);
        let mut a = Approval::for_manifest(&m);
        a.permissions.insert("tabs".to_owned());
        a.permissions.insert("not_a_permission".to_owned());
        a.hosts.insert("*://*/*".to_owned());
        a.optional_permissions.insert("scripting".to_owned());
        a.sanitize(&m);
        assert_eq!(a, Approval::for_manifest(&m));
        assert!(!a.has_permission(Permission::Tabs));
        assert!(!a.allows_url("https://evil.test/"));
    }

    #[test]
    fn carry_over_keeps_only_still_declared_optionals() {
        let old = manifest(r#","optional_permissions":["tabs","alarms"]"#);
        let mut a = Approval::for_manifest(&old);
        a.grant_optional(&old, &[Permission::Tabs, Permission::Alarms], &[]);
        let newer = manifest(r#","optional_permissions":["tabs"]"#);
        let carried = a.carry_over_optional(&newer);
        assert!(carried.optional_permissions.contains("tabs"));
        assert!(!carried.optional_permissions.contains("alarms"));
    }
}
