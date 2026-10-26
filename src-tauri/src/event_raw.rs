//! Complete upstream errors, kept outside the small journal index.
use crate::storage;
use base64::{engine::general_purpose::STANDARD, Engine};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    io::{self, Read, Write},
    path::Path,
    sync::Arc,
};

const MEMORY: usize = 2 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Header {
    pub name: String,
    pub value: String,
    #[serde(default)]
    pub base64: bool,
}
pub fn headers(values: &hyper::HeaderMap) -> Vec<Header> {
    values
        .iter()
        .map(|(name, value)| Header {
            name: name.to_string(),
            value: value
                .to_str()
                .map(str::to_owned)
                .unwrap_or_else(|_| STANDARD.encode(value.as_bytes())),
            base64: value.to_str().is_err(),
        })
        .collect()
}
pub fn header_map(values: &[Header]) -> hyper::HeaderMap {
    let mut result = hyper::HeaderMap::new();
    for h in values {
        let bytes = if h.base64 {
            STANDARD.decode(&h.value).ok()
        } else {
            Some(h.value.as_bytes().to_vec())
        };
        if let (Ok(name), Some(Ok(value))) = (
            h.name.parse::<hyper::header::HeaderName>(),
            bytes.map(|b| hyper::header::HeaderValue::from_bytes(&b)),
        ) {
            result.append(name, value);
        }
    }
    result
}

#[derive(Default)]
pub struct Capture {
    memory: Vec<u8>,
    disk: Option<std::io::BufWriter<tempfile::NamedTempFile>>,
    problem: Option<String>,
}
impl Capture {
    pub fn feed(&mut self, bytes: &[u8]) {
        if self.problem.is_some() {
            return;
        }
        if self.write(bytes).is_err() {
            self.problem = Some("错误原文暂存失败，内容不完整".into());
        }
    }
    fn write(&mut self, bytes: &[u8]) -> io::Result<()> {
        if self.disk.is_none() && self.memory.len().saturating_add(bytes.len()) > MEMORY {
            let file = storage::private_tempfile(&std::env::temp_dir())
                .map_err(|_| io::Error::other("error capture unavailable"))?;
            let mut writer = std::io::BufWriter::new(file);
            writer.write_all(&self.memory)?;
            self.memory.clear();
            self.disk = Some(writer);
        }
        if let Some(writer) = &mut self.disk {
            writer.write_all(bytes)
        } else {
            self.memory.extend_from_slice(bytes);
            Ok(())
        }
    }
    pub fn prefix(&self) -> &[u8] {
        &self.memory[..self.memory.len().min(512)]
    }
    pub fn finish(mut self, headers: Vec<Header>, event: Option<String>) -> Raw {
        let body = if let Some(mut writer) = self.disk.take() {
            if writer.flush().is_err() {
                self.problem = Some("错误原文暂存失败，内容不完整".into());
            }
            match writer.into_inner() {
                Ok(file) => Body::Disk(Arc::new(file)),
                Err(_) => {
                    self.problem = Some("错误原文暂存失败，内容不完整".into());
                    Body::Memory(Arc::new(Vec::new()))
                }
            }
        } else {
            Body::Memory(Arc::new(self.memory))
        };
        let mut raw = Raw {
            headers,
            event,
            incomplete: self.problem,
            body,
            digest: String::new(),
        };
        let mut hash = Sha256::new();
        if raw.copy_to(&mut hash).is_err() {
            raw.incomplete = Some("错误原文读取失败，内容不完整".into());
        }
        raw.digest = format!("{:x}", hash.finalize());
        raw
    }
}
#[derive(Clone, Debug)]
enum Body {
    Memory(Arc<Vec<u8>>),
    Disk(Arc<tempfile::NamedTempFile>),
}
#[derive(Clone, Debug)]
pub struct Raw {
    pub headers: Vec<Header>,
    pub event: Option<String>,
    pub incomplete: Option<String>,
    body: Body,
    digest: String,
}
impl PartialEq for Raw {
    fn eq(&self, other: &Self) -> bool {
        self.digest == other.digest
            && self.headers == other.headers
            && self.event == other.event
            && self.incomplete == other.incomplete
    }
}
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Reference {
    pub id: String,
    pub digest: String,
    pub incomplete: Option<String>,
}
#[derive(Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct View {
    pub headers: Vec<Header>,
    pub event: Option<String>,
    pub body: String,
    pub base64: bool,
    pub incomplete: Option<String>,
}
#[derive(Serialize, Deserialize)]
struct Meta {
    headers: Vec<Header>,
    event: Option<String>,
    incomplete: Option<String>,
}
impl Raw {
    pub fn bytes(bytes: &[u8], headers: Vec<Header>, event: Option<String>) -> Self {
        let mut capture = Capture::default();
        capture.feed(bytes);
        capture.finish(headers, event)
    }
    fn reader(&self) -> io::Result<Box<dyn Read + '_>> {
        match &self.body {
            Body::Memory(bytes) => Ok(Box::new(io::Cursor::new(bytes.as_slice()))),
            Body::Disk(file) => Ok(Box::new(file.reopen()?)),
        }
    }
    fn copy_to(&self, writer: &mut impl Write) -> io::Result<u64> {
        io::copy(&mut self.reader()?, writer)
    }
    pub fn value(&self) -> Option<serde_json::Value> {
        serde_json::from_reader(self.reader().ok()?).ok()
    }
    pub fn text(&self) -> io::Result<String> {
        let mut text = String::new();
        self.reader()?.read_to_string(&mut text)?;
        Ok(text)
    }
    pub fn reference(&self, id: &str) -> Reference {
        let mut hash = Sha256::new();
        hash.update(self.digest.as_bytes());
        hash.update(serde_json::to_vec(&self.headers).unwrap_or_default());
        hash.update(self.event.as_deref().unwrap_or_default().as_bytes());
        Reference {
            id: id.into(),
            digest: format!("{:x}", hash.finalize()),
            incomplete: self.incomplete.clone(),
        }
    }
    pub fn save(&self, directory: &Path, id: &str) -> storage::Result<()> {
        storage::private_dir(directory)?;
        let mut file = storage::private_tempfile(directory)?;
        let meta = Meta {
            headers: self.headers.clone(),
            event: self.event.clone(),
            incomplete: self.incomplete.clone(),
        };
        let write = || -> io::Result<()> {
            serde_json::to_writer(&mut file, &meta)?;
            file.write_all(b"\n")?;
            self.copy_to(&mut file)?;
            file.as_file().sync_all()?;
            file.persist(directory.join(format!("{id}.error")))
                .map_err(|e| e.error)?;
            Ok(())
        };
        write()
            .map_err(|_| storage::AppError::new(crate::errors::code::LOG_WRITE, "错误原文保存失败"))
    }
}
pub fn read(directory: &Path, id: &str) -> storage::Result<View> {
    let id = uuid::Uuid::parse_str(id)
        .map_err(|_| storage::AppError::new(crate::errors::code::LOG_READ, "错误原文标识无效"))?;
    let read = || -> io::Result<View> {
        use std::io::BufRead;
        let mut reader =
            io::BufReader::new(std::fs::File::open(directory.join(format!("{id}.error")))?);
        let mut line = String::new();
        reader.read_line(&mut line)?;
        let meta: Meta = serde_json::from_str(&line)?;
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes)?;
        let (body, base64) = match String::from_utf8(bytes) {
            Ok(text) => (text, false),
            Err(e) => (STANDARD.encode(e.into_bytes()), true),
        };
        Ok(View {
            headers: meta.headers,
            event: meta.event,
            body,
            base64,
            incomplete: meta.incomplete,
        })
    };
    read().map_err(|_| {
        storage::AppError::new(crate::errors::code::LOG_READ, "错误原文读取失败或已过期")
    })
}
