//! 息壤节点编解码（内核）：节点 ↔ 字节。格式见 README，所有多字节整数为大端序。

use std::fmt;

/// 大类标记（类型标记，1 字节）。
pub const EMPTY: u8 = 0; // 空：纯容器节点，无值内容
pub const INT: u8 = 1; // 整数
pub const FLOAT: u8 = 2; // 浮点数
pub const BOOL: u8 = 3; // 布尔
pub const TEXT: u8 = 4; // 文本
pub const REFERENCE: u8 = 5; // 引用：UUID，构成关联边
pub const BLOB: u8 = 6; // 二进制块：内联原始字节

/// 16 字节 UUID（节点编号 / 引用目标）。
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Uuid(pub [u8; 16]);

/// 全 0 UUID，代表「根节点」。
pub const NIL_UUID: Uuid = Uuid([0u8; 16]);

impl Uuid {
    pub fn is_nil(&self) -> bool {
        self.0 == [0u8; 16]
    }

    /// 解析 UUID 字符串（8-4-4-4-12，或去连字符的 32 位十六进制）；非法返回 None。
    pub fn parse(s: &str) -> Option<Uuid> {
        let hex: String = s.chars().filter(|c| *c != '-').collect();
        if hex.len() != 32 {
            return None;
        }
        let mut b = [0u8; 16];
        for (i, byte) in b.iter_mut().enumerate() {
            *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
        }
        Some(Uuid(b))
    }

    /// 生成随机 UUID v4（新增节点时发号）。
    pub fn random_v4() -> Uuid {
        Uuid(*uuid::Uuid::new_v4().as_bytes())
    }
}

impl fmt::Display for Uuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let b = &self.0;
        write!(
            f,
            "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7], b[8], b[9], b[10], b[11], b[12], b[13], b[14], b[15]
        )
    }
}

impl fmt::Debug for Uuid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Uuid({})", self)
    }
}

/// 节点值（7 大类）。
#[derive(Clone, PartialEq, Debug)]
pub enum Value {
    Empty,
    Int(i64),
    Float(f64),
    Bool(bool),
    Text(String),
    Reference(Uuid),
    Blob(Vec<u8>),
}

impl Value {
    pub fn tag(&self) -> u8 {
        match self {
            Value::Empty => EMPTY,
            Value::Int(_) => INT,
            Value::Float(_) => FLOAT,
            Value::Bool(_) => BOOL,
            Value::Text(_) => TEXT,
            Value::Reference(_) => REFERENCE,
            Value::Blob(_) => BLOB,
        }
    }
}

/// 节点（四属性：节点编号 → 父节点 → 节点名 → 节点值）。
#[derive(Clone, PartialEq, Debug)]
pub struct Node {
    pub id: Uuid,
    /// None = 根节点（父为全 0）。
    pub parent: Option<Uuid>,
    pub name: String,
    pub value: Value,
}

/// 解码错误。
#[derive(Debug, PartialEq)]
pub enum Error {
    /// 数据不足（文件截断）。
    Truncated,
    /// 类型标记非法。
    InvalidTag(u8),
    /// 名字 / 文本非法 UTF-8。
    InvalidUtf8,
    /// 名字超 255 字节。
    NameTooLong(usize),
}

// ---------------------------------------------------------------------------
// 编码
// ---------------------------------------------------------------------------

pub fn encode_value(v: &Value) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(v.tag());
    match v {
        Value::Empty => {}
        Value::Int(n) => out.extend_from_slice(&n.to_be_bytes()),
        Value::Float(x) => out.extend_from_slice(&x.to_be_bytes()),
        Value::Bool(b) => out.push(if *b { 1 } else { 0 }),
        Value::Text(s) => {
            let b = s.as_bytes();
            out.extend_from_slice(&(b.len() as u32).to_be_bytes());
            out.extend_from_slice(b);
        }
        Value::Reference(u) => out.extend_from_slice(&u.0),
        Value::Blob(b) => {
            out.extend_from_slice(&(b.len() as u64).to_be_bytes());
            out.extend_from_slice(b);
        }
    }
    out
}

pub fn encode_node(n: &Node) -> Result<Vec<u8>, Error> {
    let nb = n.name.as_bytes();
    if nb.len() > 255 {
        return Err(Error::NameTooLong(nb.len()));
    }
    let mut out = Vec::with_capacity(32 + nb.len());
    out.extend_from_slice(&n.id.0);
    out.extend_from_slice(&n.parent.unwrap_or(NIL_UUID).0);
    out.push(nb.len() as u8);
    out.extend_from_slice(nb);
    out.extend_from_slice(&encode_value(&n.value));
    Ok(out)
}

// ---------------------------------------------------------------------------
// 解码
// ---------------------------------------------------------------------------

fn read_bytes<'a>(data: &'a [u8], off: &mut usize, n: usize) -> Result<&'a [u8], Error> {
    if data.len() < *off + n {
        return Err(Error::Truncated);
    }
    let s = &data[*off..*off + n];
    *off += n;
    Ok(s)
}

pub fn decode_value(data: &[u8], off: &mut usize) -> Result<Value, Error> {
    let tag = *read_bytes(data, off, 1)?.first().unwrap();
    match tag {
        EMPTY => Ok(Value::Empty),
        INT => {
            let b = read_bytes(data, off, 8)?;
            Ok(Value::Int(i64::from_be_bytes(b.try_into().unwrap())))
        }
        FLOAT => {
            let b = read_bytes(data, off, 8)?;
            Ok(Value::Float(f64::from_be_bytes(b.try_into().unwrap())))
        }
        BOOL => {
            let b = read_bytes(data, off, 1)?;
            Ok(Value::Bool(b[0] != 0))
        }
        TEXT => {
            let n = u32::from_be_bytes(read_bytes(data, off, 4)?.try_into().unwrap()) as usize;
            let b = read_bytes(data, off, n)?;
            Ok(Value::Text(
                String::from_utf8(b.to_vec()).map_err(|_| Error::InvalidUtf8)?,
            ))
        }
        REFERENCE => {
            let b = read_bytes(data, off, 16)?;
            Ok(Value::Reference(Uuid(b.try_into().unwrap())))
        }
        BLOB => {
            let n = u64::from_be_bytes(read_bytes(data, off, 8)?.try_into().unwrap()) as usize;
            let b = read_bytes(data, off, n)?;
            Ok(Value::Blob(b.to_vec()))
        }
        other => Err(Error::InvalidTag(other)),
    }
}

pub fn decode_node(data: &[u8], off: &mut usize) -> Result<Node, Error> {
    let id = Uuid(read_bytes(data, off, 16)?.try_into().unwrap());
    let p = Uuid(read_bytes(data, off, 16)?.try_into().unwrap());
    let parent = if p.is_nil() { None } else { Some(p) };
    let nlen = *read_bytes(data, off, 1)?.first().unwrap() as usize;
    let name = String::from_utf8(read_bytes(data, off, nlen)?.to_vec())
        .map_err(|_| Error::InvalidUtf8)?;
    let value = decode_value(data, off)?;
    Ok(Node {
        id,
        parent,
        name,
        value,
    })
}

/// 是否为「规范十进制整数」（无前导 0，如 0 / 51975 / -3；不含 01 / 007）。
pub fn is_canonical_int(s: &str) -> bool {
    let body = s.strip_prefix('-').unwrap_or(s);
    if body.is_empty() {
        return false;
    }
    if !body.bytes().all(|b| b.is_ascii_digit()) {
        return false;
    }
    body == "0" || !body.starts_with('0')
}

/// 命令行 / 界面输入 → 节点值：整数 / 浮点 / 布尔 / 文本。
/// 「01」「007」这类前导 0 字符串按**文本**存（否则会丢成整数 1 / 7）。
pub fn parse_value(s: &str) -> Value {
    if is_canonical_int(s) {
        if let Ok(i) = s.parse::<i64>() {
            return Value::Int(i);
        }
    }
    if let Ok(f) = s.parse::<f64>() {
        if s.contains('.') || s.contains('e') || s.contains('E') {
            return Value::Float(f);
        }
    }
    if s == "true" {
        return Value::Bool(true);
    }
    if s == "false" {
        return Value::Bool(false);
    }
    Value::Text(s.to_string())
}

/// 流式节点迭代器：逐个解码节点，不一次性加载全部（大文件 / 按需解析用）。
pub struct NodeIter<'a> {
    data: &'a [u8],
    off: usize,
}

impl<'a> NodeIter<'a> {
    pub fn new(data: &'a [u8]) -> Self {
        Self { data, off: 0 }
    }
}

impl<'a> Iterator for NodeIter<'a> {
    type Item = Result<Node, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.off >= self.data.len() {
            return None;
        }
        Some(decode_node(self.data, &mut self.off))
    }
}

// ---------------------------------------------------------------------------
// 测试
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn node(id: [u8; 16], parent: Option<[u8; 16]>, name: &str, value: Value) -> Node {
        Node {
            id: Uuid(id),
            parent: parent.map(Uuid),
            name: name.into(),
            value,
        }
    }

    #[test]
    fn value_roundtrip() {
        let vals = vec![
            Value::Empty,
            Value::Int(2046),
            Value::Int(-42),
            Value::Float(1.5),
            Value::Bool(true),
            Value::Bool(false),
            Value::Text("灯".into()),
            Value::Text("hello world".into()),
            Value::Reference(Uuid([7u8; 16])),
            Value::Blob(vec![0xff, 0x00, 0x12]),
        ];
        for v in vals {
            let enc = encode_value(&v);
            let mut off = 0;
            let dec = decode_value(&enc, &mut off).unwrap();
            assert_eq!(v, dec);
            assert_eq!(off, enc.len());
        }
    }

    #[test]
    fn canonical_bytes() {
        // README 规范字节示例（对拍）
        assert_eq!(encode_value(&Value::Empty), vec![0x00]);
        assert_eq!(
            encode_value(&Value::Int(2046)),
            vec![0x01, 0, 0, 0, 0, 0, 0, 0x07, 0xfe]
        );
        assert_eq!(
            encode_value(&Value::Float(1.5)),
            vec![0x02, 0x3f, 0xf8, 0, 0, 0, 0, 0, 0]
        );
        assert_eq!(encode_value(&Value::Bool(true)), vec![0x03, 0x01]);
        assert_eq!(
            encode_value(&Value::Text("灯".into())),
            vec![0x04, 0, 0, 0, 3, 0xe7, 0x81, 0xaf]
        );
    }

    #[test]
    fn node_roundtrip() {
        let n = node(
            [1u8; 16],
            None,
            "灯",
            Value::Reference(Uuid([2u8; 16])),
        );
        let enc = encode_node(&n).unwrap();
        let mut off = 0;
        let dec = decode_node(&enc, &mut off).unwrap();
        assert_eq!(n, dec);
        assert_eq!(off, enc.len());
    }

    #[test]
    fn truncated() {
        let enc = encode_value(&Value::Text("灯".into()));
        let mut off = 0;
        assert_eq!(decode_value(&enc[..5], &mut off), Err(Error::Truncated));
    }

    #[test]
    fn invalid_tag() {
        let mut off = 0;
        assert_eq!(decode_value(&[99u8], &mut off), Err(Error::InvalidTag(99)));
    }

    #[test]
    fn name_too_long() {
        let n = node([1u8; 16], None, &"x".repeat(300), Value::Empty);
        assert!(matches!(encode_node(&n), Err(Error::NameTooLong(300))));
    }

    #[test]
    fn streaming_node_iter() {
        // 编码 3 个节点，流式迭代解码，数量一致且顺序正确
        let nodes = vec![
            node([1u8; 16], None, "a", Value::Empty),
            node([2u8; 16], Some([1u8; 16]), "b", Value::Int(42)),
            node([3u8; 16], Some([2u8; 16]), "c", Value::Text("灯".into())),
        ];
        let mut bytes = Vec::new();
        for n in &nodes {
            bytes.extend_from_slice(&encode_node(n).unwrap());
        }
        let iter = NodeIter::new(&bytes);
        let decoded: Vec<Node> = iter.map(|r| r.unwrap()).collect();
        assert_eq!(decoded.len(), 3);
        assert_eq!(decoded[1].name, "b");
        assert_eq!(decoded[2].value, Value::Text("灯".into()));
    }
}
