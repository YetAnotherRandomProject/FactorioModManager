//! Versions Factorio (x.y.z) et parsing des dépendances de info.json.

use std::fmt;
use std::str::FromStr;

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Default)]
pub struct Version(pub u32, pub u32, pub u32);

impl Version {
    pub fn major_minor(&self) -> String {
        format!("{}.{}", self.0, self.1)
    }
}

impl FromStr for Version {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let mut parts = [0u32; 3];
        let s = s.trim();
        if s.is_empty() {
            return Err("version vide".into());
        }
        for (i, p) in s.split('.').enumerate() {
            if i >= 3 {
                return Err(format!("version invalide: {s}"));
            }
            parts[i] = p
                .trim()
                .parse()
                .map_err(|_| format!("version invalide: {s}"))?;
        }
        Ok(Version(parts[0], parts[1], parts[2]))
    }
}

impl fmt::Display for Version {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}.{}.{}", self.0, self.1, self.2)
    }
}

/// Compare deux versions "majeure.mineure" de Factorio (ex. "2.0" vs "2.0.28").
pub fn same_major_minor(a: &str, b: &str) -> bool {
    match (a.parse::<Version>(), b.parse::<Version>()) {
        (Ok(a), Ok(b)) => a.0 == b.0 && a.1 == b.1,
        _ => false,
    }
}

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum DepKind {
    Required,
    RequiredNoOrder,
    /// "+" (Factorio 2.1) : optionnelle mais recommandée.
    Recommended,
    Optional,
    HiddenOptional,
    Incompatible,
}

impl DepKind {
    pub fn label(&self) -> &'static str {
        match self {
            DepKind::Required => "Obligatoire",
            DepKind::RequiredNoOrder => "Obligatoire (sans ordre)",
            DepKind::Recommended => "Recommandé",
            DepKind::Optional => "Optionnel",
            DepKind::HiddenOptional => "Optionnel caché",
            DepKind::Incompatible => "Incompatible",
        }
    }
    pub fn is_required(&self) -> bool {
        matches!(self, DepKind::Required | DepKind::RequiredNoOrder)
    }
    /// Groupe affiché dans l'UI : 0 obligatoire, 1 recommandé, 2 optionnel, 3 optionnel caché, 4 incompatible.
    pub fn group(&self) -> i32 {
        match self {
            DepKind::Required | DepKind::RequiredNoOrder => 0,
            DepKind::Recommended => 1,
            DepKind::Optional => 2,
            DepKind::HiddenOptional => 3,
            DepKind::Incompatible => 4,
        }
    }
    pub fn is_optional_like(&self) -> bool {
        matches!(self, DepKind::Recommended | DepKind::Optional | DepKind::HiddenOptional)
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Op {
    Lt,
    Le,
    Eq,
    Ge,
    Gt,
}

impl Op {
    fn as_str(&self) -> &'static str {
        match self {
            Op::Lt => "<",
            Op::Le => "<=",
            Op::Eq => "=",
            Op::Ge => ">=",
            Op::Gt => ">",
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Dependency {
    pub kind: DepKind,
    pub name: String,
    pub constraint: Option<(Op, Version)>,
}

impl Dependency {
    pub fn parse(raw: &str) -> Option<Dependency> {
        let s = raw.trim();
        let (kind, rest) = if let Some(r) = s.strip_prefix("(?)") {
            (DepKind::HiddenOptional, r)
        } else if let Some(r) = s.strip_prefix('+') {
            (DepKind::Recommended, r)
        } else if let Some(r) = s.strip_prefix('!') {
            (DepKind::Incompatible, r)
        } else if let Some(r) = s.strip_prefix('?') {
            (DepKind::Optional, r)
        } else if let Some(r) = s.strip_prefix('~') {
            (DepKind::RequiredNoOrder, r)
        } else {
            (DepKind::Required, s)
        };
        let rest = rest.trim();
        let op_pos = rest.find(['<', '>', '=']);
        let (name, constraint) = match op_pos {
            None => (rest.to_string(), None),
            Some(pos) => {
                let name = rest[..pos].trim().to_string();
                let tail = &rest[pos..];
                let (op, ver) = if let Some(v) = tail.strip_prefix("<=") {
                    (Op::Le, v)
                } else if let Some(v) = tail.strip_prefix(">=") {
                    (Op::Ge, v)
                } else if let Some(v) = tail.strip_prefix('<') {
                    (Op::Lt, v)
                } else if let Some(v) = tail.strip_prefix('>') {
                    (Op::Gt, v)
                } else if let Some(v) = tail.strip_prefix('=') {
                    (Op::Eq, v)
                } else {
                    return None;
                };
                let ver: Version = ver.trim().parse().ok()?;
                (name, Some((op, ver)))
            }
        };
        if name.is_empty() {
            return None;
        }
        Some(Dependency { kind, name, constraint })
    }

    pub fn matches(&self, v: &Version) -> bool {
        match &self.constraint {
            None => true,
            Some((op, req)) => match op {
                Op::Lt => v < req,
                Op::Le => v <= req,
                Op::Eq => v == req,
                Op::Ge => v >= req,
                Op::Gt => v > req,
            },
        }
    }

    pub fn constraint_str(&self) -> String {
        match &self.constraint {
            None => String::new(),
            Some((op, v)) => format!("{} {}", op.as_str(), v),
        }
    }
}

/// Mods fournis avec le jeu (jamais téléchargés depuis le portail).
pub const BUILTIN_MODS: &[&str] = &["base", "core", "space-age", "quality", "elevated-rails", "recycler"];

pub fn is_builtin(name: &str, extra: &[String]) -> bool {
    BUILTIN_MODS.contains(&name) || extra.iter().any(|e| e == name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_versions() {
        assert_eq!("1.2.3".parse::<Version>().unwrap(), Version(1, 2, 3));
        assert_eq!("2.0".parse::<Version>().unwrap(), Version(2, 0, 0));
        assert!("a.b".parse::<Version>().is_err());
        assert!(Version(1, 10, 0) > Version(1, 9, 9));
        assert!(same_major_minor("2.0", "2.0.28"));
        assert!(!same_major_minor("2.1", "2.0.28"));
    }

    #[test]
    fn parse_deps() {
        let d = Dependency::parse("base >= 2.0").unwrap();
        assert_eq!(d.kind, DepKind::Required);
        assert_eq!(d.name, "base");
        assert!(d.matches(&Version(2, 0, 5)));
        assert!(!d.matches(&Version(1, 1, 0)));

        let d = Dependency::parse("? bobtech").unwrap();
        assert_eq!(d.kind, DepKind::Optional);
        assert_eq!(d.name, "bobtech");
        assert!(d.constraint.is_none());

        let d = Dependency::parse("(?) foo-bar < 1.2").unwrap();
        assert_eq!(d.kind, DepKind::HiddenOptional);
        assert_eq!(d.constraint, Some((Op::Lt, Version(1, 2, 0))));

        let d = Dependency::parse("!space-exploration").unwrap();
        assert_eq!(d.kind, DepKind::Incompatible);

        let d = Dependency::parse("~ flib>=0.12.0").unwrap();
        assert_eq!(d.kind, DepKind::RequiredNoOrder);
        assert_eq!(d.name, "flib");
        assert_eq!(d.constraint, Some((Op::Ge, Version(0, 12, 0))));

        let d = Dependency::parse("+MilitaryMK2SciencePack").unwrap();
        assert_eq!(d.kind, DepKind::Recommended);
        assert_eq!(d.name, "MilitaryMK2SciencePack");
        let d = Dependency::parse("+ space-age").unwrap();
        assert_eq!((d.kind, d.name.as_str()), (DepKind::Recommended, "space-age"));

        let d = Dependency::parse("mod with space = 1.0.0").unwrap();
        assert_eq!(d.name, "mod with space");
    }
}
