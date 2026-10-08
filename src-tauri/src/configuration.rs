use crate::{
    gateway::ClientId,
    storage::{self, AppError, Result},
};
use serde::{
    de::{self, MapAccess, SeqAccess, Visitor},
    Deserialize, Deserializer, Serialize,
};
use serde_json::Value;
use std::{collections::HashSet, fmt, path::Path};

#[derive(Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Document {
    pub client_id: ClientId,
    pub path: String,
    pub text: String,
    pub revision: String,
    pub guarded: bool,
    pub can_restore: bool,
}

// Deserialize every object explicitly: Value alone silently accepts duplicate keys.
struct Unique;
impl<'de> Deserialize<'de> for Unique {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct Check;
        impl<'de> Visitor<'de> for Check {
            type Value = Unique;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("JSON 值")
            }
            fn visit_map<M: MapAccess<'de>>(
                self,
                mut m: M,
            ) -> std::result::Result<Unique, M::Error> {
                let mut keys = HashSet::new();
                while let Some(k) = m.next_key::<String>()? {
                    if !keys.insert(k) {
                        return Err(de::Error::custom("配置包含重复字段"));
                    }
                    m.next_value::<Unique>()?;
                }
                Ok(Unique)
            }
            fn visit_seq<S: SeqAccess<'de>>(
                self,
                mut s: S,
            ) -> std::result::Result<Unique, S::Error> {
                while s.next_element::<Unique>()?.is_some() {}
                Ok(Unique)
            }
            fn visit_bool<E: de::Error>(self, _: bool) -> std::result::Result<Unique, E> {
                Ok(Unique)
            }
            fn visit_str<E: de::Error>(self, _: &str) -> std::result::Result<Unique, E> {
                Ok(Unique)
            }
            fn visit_i64<E: de::Error>(self, _: i64) -> std::result::Result<Unique, E> {
                Ok(Unique)
            }
            fn visit_u64<E: de::Error>(self, _: u64) -> std::result::Result<Unique, E> {
                Ok(Unique)
            }
            fn visit_f64<E: de::Error>(self, _: f64) -> std::result::Result<Unique, E> {
                Ok(Unique)
            }
            fn visit_unit<E: de::Error>(self) -> std::result::Result<Unique, E> {
                Ok(Unique)
            }
        }
        d.deserialize_any(Check)
    }
}
fn json_error(e: serde_json::Error) -> AppError {
    // Parser errors can echo source values; return position without the payload.
    AppError {
        code: "JSON".into(),
        message: if e.to_string().starts_with("配置包含重复字段") {
            "配置包含重复字段".into()
        } else {
            "JSON 格式无效".into()
        },
        line: Some(e.line()),
        column: Some(e.column()),
    }
}
pub(crate) fn parse_object(text: &str) -> Result<Value> {
    serde_json::from_str::<Unique>(text).map_err(json_error)?;
    let v: Value = serde_json::from_str(text).map_err(json_error)?;
    if !v.is_object() {
        return Err(AppError::new("JSON", "配置根节点必须是对象"));
    }
    Ok(v)
}
pub fn validate(client: ClientId, text: &str) -> Result<()> {
    if client == ClientId::Codex {
        return crate::core::validate_config(text);
    }
    let v = parse_object(text)?;
    let check = |path: &str, valid: fn(&Value) -> bool, label: &str| -> Result<()> {
        if v.pointer(path).is_some_and(|x| !valid(x)) {
            return Err(AppError::new("CONFIG_TYPE", &format!("{label}类型无效")));
        }
        Ok(())
    };
    for key in ["model", "language"] {
        check(&format!("/{key}"), Value::is_string, key)?;
    }
    for key in [
        "env",
        "permissions",
        "sandbox",
        "attribution",
        "enabledPlugins",
        "hooks",
        "modelSettings",
    ] {
        check(&format!("/{key}"), Value::is_object, key)?;
    }
    for key in [
        "alwaysThinkingEnabled",
        "autoMemoryEnabled",
        "spinnerTipsEnabled",
        "showTurnDuration",
        "prefersReducedMotion",
    ] {
        check(&format!("/{key}"), Value::is_boolean, key)?;
    }
    check("/sandbox/enabled", Value::is_boolean, "沙箱")?;
    check("/permissions/defaultMode", Value::is_string, "权限模式")?;
    for key in ["allow", "ask", "deny", "additionalDirectories"] {
        check(
            &format!("/permissions/{key}"),
            |x| x.as_array().is_some_and(|a| a.iter().all(Value::is_string)),
            "权限规则",
        )?;
    }
    for key in ["commit", "pr"] {
        check(&format!("/attribution/{key}"), Value::is_string, "署名")?;
    }
    if v.get("env")
        .and_then(Value::as_object)
        .is_some_and(|e| e.values().any(|x| !x.is_string()))
    {
        return Err(AppError::new("CONFIG_TYPE", "环境变量的值必须是字符串"));
    }
    Ok(())
}
fn backup(client: ClientId, data: &Path) -> std::path::PathBuf {
    data.join(if client == ClientId::Codex {
        "previous-config.toml"
    } else {
        "previous-settings.json"
    })
}
pub fn read(client: ClientId, home: &Path, data: &Path, guarded: bool) -> Result<Document> {
    let path = client.config(home);
    let raw = storage::read_optional(&path)?;
    let revision = storage::revision(raw.as_deref());
    let text = String::from_utf8(raw.unwrap_or_else(|| {
        if client == ClientId::Claude {
            b"{}\n".to_vec()
        } else {
            Vec::new()
        }
    }))
    .map_err(|_| AppError::new("UTF8", "配置必须使用 UTF-8 编码"))?;
    Ok(Document {
        client_id: client,
        text,
        revision,
        path: path.to_string_lossy().into(),
        guarded,
        can_restore: backup(client, data).is_file(),
    })
}
pub fn previous(client: ClientId, data: &Path) -> Result<String> {
    let bytes = storage::read_optional(&backup(client, data))?
        .ok_or_else(|| AppError::new("RESTORE", "没有可恢复的配置"))?;
    String::from_utf8(bytes).map_err(|_| AppError::new("UTF8", "历史配置不是 UTF-8"))
}
pub fn save(
    client: ClientId,
    home: &Path,
    data: &Path,
    text: &str,
    expected: &str,
    guarded: bool,
) -> Result<Document> {
    validate(client, text)?;
    let path = client.config(home);
    let previous = storage::read_optional(&path)?;
    if storage::revision(previous.as_deref()) != expected {
        return Err(AppError::new(
            "CONFLICT",
            "配置已被其他程序修改，草稿已保留。请重新读取后合并",
        ));
    }
    if let Some(bytes) = previous {
        storage::atomic_write(&backup(client, data), &bytes, None)?;
    }
    storage::atomic_write(&path, text.as_bytes(), Some(expected))?;
    read(client, home, data, guarded)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn strict_json_and_unknown_fields() {
        for text in [
            "[]",
            "{\"env\":[],\"env\":{}}",
            "{\"x\":{\"a\":1,\"a\":2}}",
            "{\"x\":1,}",
            "{\"env\":{\"KEY\":4}}",
            "{\"sandbox\":{\"enabled\":0}}",
        ] {
            assert!(validate(ClientId::Claude, text).is_err(), "{text}");
        }
        validate(ClientId::Claude, r#"{"future":{"zero":0,"empty":"","off":false},"env":{"X":"0"},"modelSettings":{"unknown":{"effortLevel":"future"}}}"#).unwrap();
        let error = validate(ClientId::Claude, "{\"env\": SECRET_TOKEN}").unwrap_err();
        assert!(!error.message.contains("SECRET_TOKEN"));
        assert!(error.line.is_some());
    }
    #[test]
    fn raw_roundtrip_conflict_and_backup_are_client_local() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("claude");
        let data = t.path().join("data");
        storage::private_dir(&data).unwrap();
        let text = "{\r\n  \"env\": {\"X\": \"0\"},\r\n  \"unknown\": false\r\n}\r\n";
        let first = save(ClientId::Claude, &home, &data, text, "missing", false).unwrap();
        assert_eq!(first.text, text);
        assert!(!first.can_restore);
        let next = save(ClientId::Claude, &home, &data, "{}", &first.revision, false).unwrap();
        assert!(next.can_restore);
        assert_eq!(previous(ClientId::Claude, &data).unwrap(), text);
        assert_eq!(
            save(ClientId::Claude, &home, &data, text, &first.revision, false)
                .err()
                .unwrap()
                .code,
            "CONFLICT"
        );
        assert!(!home.join("config.toml").exists());
        assert!(!home.join("auth.json").exists());
    }

    #[test]
    fn failed_backup_does_not_write_config_and_saved_files_are_private() {
        let t = tempfile::tempdir().unwrap();
        let home = t.path().join("home");
        let data = t.path().join("data");
        storage::private_dir(&data).unwrap();
        let first = save(ClientId::Claude, &home, &data, "{}\n", "missing", false).unwrap();
        std::fs::create_dir(backup(ClientId::Claude, &data)).unwrap();
        assert!(save(
            ClientId::Claude,
            &home,
            &data,
            "{\"language\":\"中文\"}",
            &first.revision,
            false
        )
        .is_err());
        assert_eq!(
            std::fs::read_to_string(home.join("settings.json")).unwrap(),
            first.text
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(home.join("settings.json"))
                    .unwrap()
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            assert_eq!(
                std::fs::metadata(&data).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
    }
}
