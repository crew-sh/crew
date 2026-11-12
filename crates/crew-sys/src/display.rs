use std::collections::BTreeMap;

const ALWAYS: [&str; 2] = ["WAYLAND_DISPLAY", "WAYLAND_SOCKET"];
const OWNER_X_DISPLAYS: [&str; 2] = [":0", ":1"];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Desktop {
    Denied,
    Allowed,
}

/// Removes the owner's display from `env` and returns the removed keys, sorted.
pub fn sanitize(env: &mut BTreeMap<String, String>, desktop: Desktop) -> Vec<String> {
    if desktop == Desktop::Allowed {
        return Vec::new();
    }
    let mut removed = Vec::new();
    for key in ALWAYS {
        if env.remove(key).is_some() {
            removed.push(key.to_string());
        }
    }
    if env.get("DISPLAY").is_some_and(|d| OWNER_X_DISPLAYS.contains(&d.as_str())) {
        env.remove("DISPLAY");
        removed.push("DISPLAY".to_string());
    }
    removed.sort();
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| ((*k).to_string(), (*v).to_string())).collect()
    }

    #[test]
    fn strips_wayland_and_owner_x_displays() {
        for display in [":0", ":1"] {
            let mut e = env(&[("WAYLAND_DISPLAY", "wayland-1"), ("WAYLAND_SOCKET", "3"), ("DISPLAY", display), ("HOME", "/h")]);
            let removed = sanitize(&mut e, Desktop::Denied);
            assert_eq!(removed, ["DISPLAY", "WAYLAND_DISPLAY", "WAYLAND_SOCKET"]);
            assert_eq!(e, env(&[("HOME", "/h")]));
        }
    }

    #[test]
    fn keeps_private_x_display() {
        let mut e = env(&[("DISPLAY", ":99")]);
        assert!(sanitize(&mut e, Desktop::Denied).is_empty());
        assert_eq!(e, env(&[("DISPLAY", ":99")]));
    }

    #[test]
    fn desktop_override_keeps_everything() {
        let mut e = env(&[("WAYLAND_DISPLAY", "wayland-1"), ("DISPLAY", ":0")]);
        assert!(sanitize(&mut e, Desktop::Allowed).is_empty());
        assert_eq!(e.len(), 2);
    }
}
