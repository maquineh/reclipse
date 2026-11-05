//! Minimal Java class file reader used by the classpath index.
//!
//! It extracts public/protected type and member metadata. Bytecode, annotations
//! and generic signatures are deliberately not interpreted yet.

use anyhow::{bail, Result};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TypeKind {
    Class,
    Interface,
    Enum,
    Annotation,
}

impl TypeKind {
    pub fn label(self) -> &'static str {
        match self {
            Self::Class => "class",
            Self::Interface => "interface",
            Self::Enum => "enum",
            Self::Annotation => "annotation",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberKind {
    Field,
    Method,
    Constructor,
}

#[derive(Debug, Clone)]
pub struct MemberInfo {
    pub name: String,
    pub descriptor: String,
    /// Human-readable form, e.g. `String name` or `int size()`.
    pub signature: String,
    pub kind: MemberKind,
    pub is_static: bool,
    pub is_public: bool,
    /// JVM `ACC_VARARGS` flag; meaningful for methods and constructors.
    pub is_varargs: bool,
    /// JVM generic signature (`Signature` attribute), e.g.
    /// `(TT;)TT;` for `<T> T identity(T)`. `None` when absent.
    pub generic_signature: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ClassInfo {
    /// Fully qualified name with `.` separators (nested types keep `$`).
    pub name: String,
    pub display_name: String,
    pub package: String,
    pub kind: TypeKind,
    pub is_public: bool,
    pub super_name: Option<String>,
    pub interfaces: Vec<String>,
    pub fields: Vec<MemberInfo>,
    pub methods: Vec<MemberInfo>,
    /// JVM generic signature of the class (`Signature` attribute), e.g.
    /// `<T::Ljava/lang/Object;>Ljava/lang/Object;`.
    pub generic_signature: Option<String>,
    /// Human-readable origin (JAR/JMOD path).
    pub origin: String,
}

impl ClassInfo {
    pub fn simple_name(&self) -> &str {
        self.name.rsplit('.').next().unwrap_or(&self.name)
    }
}

/// Parse one `.class` payload.
pub fn parse_class(data: &[u8], origin: impl Into<String>) -> Result<ClassInfo> {
    let mut reader = Reader::new(data);
    if reader.u4()? != 0xCAFE_BABE {
        bail!("not a Java class file");
    }
    let _minor = reader.u2()?;
    let _major = reader.u2()?;
    let constant_pool_count = reader.u2()? as usize;
    if constant_pool_count == 0 {
        bail!("invalid constant pool");
    }

    let mut utf8: Vec<Option<String>> = vec![None; constant_pool_count];
    let mut class_names: Vec<Option<u16>> = vec![None; constant_pool_count];
    let mut index = 1;
    while index < constant_pool_count {
        let tag = reader.u1()?;
        match tag {
            1 => {
                let length = reader.u2()? as usize;
                let bytes = reader.take(length)?;
                utf8[index] = Some(String::from_utf8_lossy(bytes).into_owned());
            }
            3 | 4 | 9 | 10 | 11 | 12 | 17 | 18 => reader.skip(4)?,
            5 | 6 => {
                reader.skip(8)?;
                index += 1;
            }
            7 => class_names[index] = Some(reader.u2()?),
            8 | 16 | 19 | 20 => reader.skip(2)?,
            15 => reader.skip(3)?,
            other => bail!("unsupported constant pool tag {other}"),
        }
        index += 1;
    }

    let access = reader.u2()?;
    let this_class = reader.u2()?;
    let super_class = reader.u2()?;
    let interface_count = reader.u2()? as usize;
    let mut interfaces = Vec::with_capacity(interface_count);
    for _ in 0..interface_count {
        if let Some(name) = class_name(reader.u2()?, &class_names, &utf8) {
            interfaces.push(normalize_internal_name(&name));
        }
    }

    let internal_name = class_name(this_class, &class_names, &utf8)
        .ok_or_else(|| anyhow::anyhow!("missing class name"))?;
    if internal_name.ends_with("module-info") || internal_name.ends_with("package-info") {
        bail!("skipping module/package info");
    }
    let name = internal_name.replace('/', ".");
    let package = name
        .rsplit_once('.')
        .map(|(package, _)| package.to_owned())
        .unwrap_or_default();
    let display_name = name.rsplit('.').next().unwrap_or(&name).replace('$', ".");
    let kind = if access & 0x2000 != 0 {
        TypeKind::Annotation
    } else if access & 0x0200 != 0 {
        TypeKind::Interface
    } else if access & 0x4000 != 0 {
        TypeKind::Enum
    } else {
        TypeKind::Class
    };
    let super_name =
        class_name(super_class, &class_names, &utf8).map(|name| normalize_internal_name(&name));

    let field_count = reader.u2()? as usize;
    let mut fields = Vec::new();
    for _ in 0..field_count {
        let member_access = reader.u2()?;
        let member_name = utf8_at(reader.u2()?, &utf8).unwrap_or_default();
        let descriptor = utf8_at(reader.u2()?, &utf8).unwrap_or_default();
        let generic_signature = attribute_signature(&mut reader, &utf8)?;
        if member_access & 0x0005 == 0 {
            continue;
        }
        fields.push(MemberInfo {
            name: member_name.clone(),
            signature: format!("{} {member_name}", format_field_descriptor(&descriptor)),
            descriptor,
            generic_signature,
            kind: MemberKind::Field,
            is_static: member_access & 0x0008 != 0,
            is_public: member_access & 0x0001 != 0 || member_access & 0x0004 != 0,
            is_varargs: false,
        });
    }

    let method_count = reader.u2()? as usize;
    let mut methods = Vec::new();
    for _ in 0..method_count {
        let member_access = reader.u2()?;
        let member_name = utf8_at(reader.u2()?, &utf8).unwrap_or_default();
        let descriptor = utf8_at(reader.u2()?, &utf8).unwrap_or_default();
        let generic_signature = attribute_signature(&mut reader, &utf8)?;
        if member_name == "<clinit>" || member_access & 0x0005 == 0 {
            continue;
        }
        let (kind, signature) = if member_name == "<init>" {
            (
                MemberKind::Constructor,
                format!("{}({})", display_name, format_method_arguments(&descriptor)),
            )
        } else {
            let (arguments, return_type) = format_method_descriptor(&descriptor);
            (
                MemberKind::Method,
                format!("{return_type} {member_name}({arguments})"),
            )
        };
        methods.push(MemberInfo {
            name: member_name,
            signature,
            descriptor,
            generic_signature,
            kind,
            is_static: member_access & 0x0008 != 0,
            is_public: member_access & 0x0001 != 0 || member_access & 0x0004 != 0,
            is_varargs: member_access & 0x0080 != 0,
        });
    }

    // Class-level attributes (Signature included); tolerate truncation.
    let generic_signature = match (|| -> Result<Option<String>> {
        let mut signature = None;
        let count = reader.u2()? as usize;
        for _ in 0..count {
            let name_index = reader.u2()?;
            let length = reader.u4()? as usize;
            if utf8_at(name_index, &utf8).as_deref() == Some("Signature") {
                let bytes = reader.take(length)?;
                if signature.is_none() {
                    signature = Some(String::from_utf8_lossy(bytes).into_owned());
                }
            } else {
                reader.skip(length)?;
            }
        }
        Ok(signature)
    })() {
        Ok(signature) => signature,
        Err(_) => None,
    };

    Ok(ClassInfo {
        name,
        display_name,
        package,
        kind,
        is_public: access & 0x0001 != 0,
        super_name,
        interfaces,
        fields,
        methods,
        generic_signature,
        origin: origin.into(),
    })
}

fn class_name(index: u16, class_names: &[Option<u16>], utf8: &[Option<String>]) -> Option<String> {
    let name_index = class_names.get(index as usize).copied().flatten()?;
    utf8_at(name_index, utf8)
}

fn utf8_at(index: u16, utf8: &[Option<String>]) -> Option<String> {
    utf8.get(index as usize).cloned().flatten()
}

fn normalize_internal_name(name: &str) -> String {
    name.replace('/', ".")
}

/// Read member attributes, returning the `Signature` attribute when present.
/// Other attributes are skipped unread.
fn attribute_signature(reader: &mut Reader<'_>, utf8: &[Option<String>]) -> Result<Option<String>> {
    let count = reader.u2()? as usize;
    let mut signature = None;
    for _ in 0..count {
        let name_index = reader.u2()?;
        let length = reader.u4()? as usize;
        if utf8_at(name_index, utf8).as_deref() == Some("Signature") {
            let bytes = reader.take(length)?;
            if signature.is_none() {
                signature = Some(String::from_utf8_lossy(bytes).into_owned());
            }
        } else {
            reader.skip(length)?;
        }
    }
    Ok(signature)
}

/// Format a field descriptor such as `Ljava/lang/String;`.
pub fn format_field_descriptor(descriptor: &str) -> String {
    let mut index = 0;
    parse_type(descriptor.as_bytes(), &mut index)
}

/// Return the fully qualified field type (`java.lang.String`) when the
/// descriptor refers to a class. Primitive and array descriptors return the
/// readable form.
pub fn field_type_name(descriptor: &str) -> Option<String> {
    let mut index = 0;
    let name = parse_full_type(descriptor.as_bytes(), &mut index);
    if name.is_empty() || name == "?" {
        None
    } else {
        Some(name)
    }
}

/// Return the fully qualified return type of a method descriptor.
pub fn method_return_type_name(descriptor: &str) -> Option<String> {
    let bytes = descriptor.as_bytes();
    let mut index = 0;
    if bytes.get(index) == Some(&b'(') {
        index += 1;
    }
    while index < bytes.len() && bytes[index] != b')' {
        let _ = parse_full_type(bytes, &mut index);
    }
    if bytes.get(index) == Some(&b')') {
        index += 1;
    }
    if index >= bytes.len() {
        return None;
    }
    let name = parse_full_type(bytes, &mut index);
    if name.is_empty() || name == "void" || name == "?" {
        None
    } else {
        Some(name)
    }
}

/// Parse a JVM generic `Signature` type parameter section, e.g.
/// `<T::Ljava/lang/Object;E:Ljava/lang/Number;>` into
/// `[(T, None), (E, Some(java.lang.Number))]`.
/// Bounds keep dots (`/` and `$`-style inner `.` normalized to `.`).
pub fn signature_type_params(signature: &str) -> Vec<(String, Option<String>)> {
    let bytes = signature.as_bytes();
    if bytes.first() != Some(&b'<') {
        return Vec::new();
    }
    let mut index = 1;
    let mut params = Vec::new();
    while index < bytes.len() && bytes[index] != b'>' {
        let start = index;
        while index < bytes.len() && bytes[index] != b':' {
            index += 1;
        }
        if index >= bytes.len() {
            break;
        }
        let name = String::from_utf8_lossy(&bytes[start..index]).into_owned();
        index += 1; // class bound ':'
        let mut bounds: Vec<String> = Vec::new();
        // Class bound may be empty (just ':'); interface bounds add more.
        loop {
            if index >= bytes.len() || bytes[index] == b'>' {
                break;
            }
            if bytes[index] == b':' {
                // Empty class bound, move to interface bounds.
                index += 1;
                continue;
            }
            // A new type parameter starts with an identifier followed by ':'.
            // If we already have no bound and see `Name:`, this param is done.
            if bounds.is_empty()
                && index < bytes.len()
                && (bytes[index].is_ascii_alphanumeric()
                    || bytes[index] == b'_'
                    || bytes[index] == b'$')
            {
                let scan = index;
                let mut end = scan;
                while end < bytes.len()
                    && (bytes[end].is_ascii_alphanumeric()
                        || bytes[end] == b'_'
                        || bytes[end] == b'$')
                {
                    end += 1;
                }
                if bytes.get(end) == Some(&b':') && end > scan {
                    break;
                }
            }
            match parse_signature_type(bytes, &mut index) {
                Some(bound) if !bound.is_empty() => bounds.push(bound),
                _ => break,
            }
            if index < bytes.len() && bytes[index] == b':' {
                index += 1;
                continue;
            }
            // Another bound only follows a ':'; otherwise next param or '>'.
            break;
        }
        if name.is_empty() {
            continue;
        }
        params.push((name, bounds.into_iter().next()));
    }
    params
}

/// Parameter types of a generic method `Signature` as display text, e.g.
/// `(TT;I)Ljava/lang/String;` -> [`T`, `int`].
pub fn signature_params_text(signature: &str) -> Option<Vec<String>> {
    let bytes = signature.as_bytes();
    let mut index = 0;
    if bytes.first() == Some(&b'<') {
        let mut depth = 0i32;
        while index < bytes.len() {
            match bytes[index] {
                b'<' => depth += 1,
                b'>' => {
                    depth -= 1;
                    index += 1;
                    if depth == 0 {
                        break;
                    }
                    continue;
                }
                _ => {}
            }
            index += 1;
        }
    }
    if bytes.get(index) != Some(&b'(') {
        return None;
    }
    index += 1;
    let mut params = Vec::new();
    while index < bytes.len() && bytes[index] != b')' {
        params.push(parse_signature_type(bytes, &mut index)?);
    }
    Some(params)
}

/// Parse the return type of a generic method `Signature` into display text
/// with dots, e.g. `(TT;)TT;` -> `T`, `(I)Ljava/util/List<TT;>;` ->
/// `java.util.List<T>`. Type arguments keep single-letter variables as-is.
pub fn signature_return_text(signature: &str) -> Option<String> {
    let bytes = signature.as_bytes();
    let after_params = skip_signature_params(bytes, index0(bytes))?;
    let mut index = after_params;
    parse_signature_type(bytes, &mut index)
}

fn index0(bytes: &[u8]) -> usize {
    let mut index = 0;
    if bytes.first() == Some(&b'<') {
        let mut depth = 0i32;
        while index < bytes.len() {
            match bytes[index] {
                b'<' => depth += 1,
                b'>' => {
                    depth -= 1;
                    index += 1;
                    if depth == 0 {
                        break;
                    }
                    continue;
                }
                _ => {}
            }
            index += 1;
        }
    }
    index
}

fn skip_signature_params(bytes: &[u8], mut index: usize) -> Option<usize> {
    if bytes.get(index) != Some(&b'(') {
        return None;
    }
    index += 1;
    let mut depth = 0i32;
    while index < bytes.len() {
        match bytes[index] {
            b'(' | b'[' => depth += 1,
            b'<' => depth += 1,
            b')' => {
                if depth == 0 {
                    return Some(index + 1);
                }
                depth -= 1;
            }
            b'>' | b']' => depth = (depth - 1).max(0),
            b';' => {}
            _ => {}
        }
        index += 1;
    }
    None
}

/// Parse one JVM generic signature type starting at `index`, advancing past it.
/// Returns display text with `.` separators; `Inner` classes keep `.`.
fn parse_signature_type(bytes: &[u8], index: &mut usize) -> Option<String> {
    let byte = *bytes.get(*index)?;
    match byte {
        b'B' => {
            *index += 1;
            Some("byte".to_owned())
        }
        b'C' => {
            *index += 1;
            Some("char".to_owned())
        }
        b'D' => {
            *index += 1;
            Some("double".to_owned())
        }
        b'F' => {
            *index += 1;
            Some("float".to_owned())
        }
        b'I' => {
            *index += 1;
            Some("int".to_owned())
        }
        b'J' => {
            *index += 1;
            Some("long".to_owned())
        }
        b'S' => {
            *index += 1;
            Some("short".to_owned())
        }
        b'Z' => {
            *index += 1;
            Some("boolean".to_owned())
        }
        b'V' => {
            *index += 1;
            Some("void".to_owned())
        }
        b'[' => {
            *index += 1;
            parse_signature_type(bytes, index).map(|inner| format!("{inner}[]"))
        }
        b'T' => {
            *index += 1;
            let start = *index;
            while *index < bytes.len() && bytes[*index] != b';' {
                *index += 1;
            }
            let name = String::from_utf8_lossy(&bytes[start..*index]).into_owned();
            if bytes.get(*index) == Some(&b';') {
                *index += 1;
            }
            if name.is_empty() {
                return None;
            }
            Some(name)
        }
        b'L' => {
            *index += 1;
            let mut name = String::new();
            let mut args: Vec<String> = Vec::new();
            loop {
                if *index >= bytes.len() {
                    return None;
                }
                match bytes[*index] {
                    b';' => {
                        *index += 1;
                        break;
                    }
                    b'<' => {
                        *index += 1;
                        loop {
                            if *index >= bytes.len() {
                                return None;
                            }
                            match bytes[*index] {
                                b'>' => {
                                    *index += 1;
                                    break;
                                }
                                b'*' => {
                                    *index += 1;
                                    args.push("?".to_owned());
                                }
                                b'+' | b'-' => {
                                    *index += 1;
                                    if let Some(bound) = parse_signature_type(bytes, index) {
                                        args.push(bound);
                                    }
                                }
                                _ => {
                                    if let Some(arg) = parse_signature_type(bytes, index) {
                                        args.push(arg);
                                    } else {
                                        return None;
                                    }
                                }
                            }
                        }
                    }
                    b'.' => {
                        name.push('.');
                        *index += 1;
                    }
                    b'/' => {
                        name.push('.');
                        *index += 1;
                    }
                    _ => {
                        name.push(bytes[*index] as char);
                        *index += 1;
                    }
                }
            }
            if name.is_empty() {
                return None;
            }
            if args.is_empty() {
                Some(name)
            } else {
                Some(format!("{name}<{}>", args.join(", ")))
            }
        }
        _ => None,
    }
}

/// Return `(arguments, return_type)` for a method descriptor.
pub fn format_method_descriptor(descriptor: &str) -> (String, String) {
    let bytes = descriptor.as_bytes();
    let mut index = 0;
    if bytes.get(index) == Some(&b'(') {
        index += 1;
    }
    let mut arguments = Vec::new();
    while index < bytes.len() && bytes[index] != b')' {
        arguments.push(parse_type(bytes, &mut index));
    }
    if bytes.get(index) == Some(&b')') {
        index += 1;
    }
    let return_type = if index < bytes.len() {
        parse_type(bytes, &mut index)
    } else {
        "void".to_owned()
    };
    (arguments.join(", "), return_type)
}

pub fn format_method_arguments(descriptor: &str) -> String {
    format_method_descriptor(descriptor).0
}

fn parse_type(bytes: &[u8], index: &mut usize) -> String {
    if *index >= bytes.len() {
        return "?".to_owned();
    }
    match bytes[*index] {
        b'B' => {
            *index += 1;
            "byte".to_owned()
        }
        b'C' => {
            *index += 1;
            "char".to_owned()
        }
        b'D' => {
            *index += 1;
            "double".to_owned()
        }
        b'F' => {
            *index += 1;
            "float".to_owned()
        }
        b'I' => {
            *index += 1;
            "int".to_owned()
        }
        b'J' => {
            *index += 1;
            "long".to_owned()
        }
        b'S' => {
            *index += 1;
            "short".to_owned()
        }
        b'Z' => {
            *index += 1;
            "boolean".to_owned()
        }
        b'V' => {
            *index += 1;
            "void".to_owned()
        }
        b'[' => {
            *index += 1;
            format!("{}[]", parse_type(bytes, index))
        }
        b'L' => {
            *index += 1;
            let start = *index;
            while *index < bytes.len() && bytes[*index] != b';' {
                *index += 1;
            }
            let name = String::from_utf8_lossy(&bytes[start..*index]).into_owned();
            if bytes.get(*index) == Some(&b';') {
                *index += 1;
            }
            let simple = name
                .replace('/', ".")
                .rsplit('.')
                .next()
                .unwrap_or(&name)
                .replace('$', ".");
            simple
        }
        _ => {
            *index += 1;
            "?".to_owned()
        }
    }
}

/// Like `parse_type`, but keeps the fully qualified class name. Used by the
/// semantic layer to follow member types.
fn parse_full_type(bytes: &[u8], index: &mut usize) -> String {
    if *index >= bytes.len() {
        return String::new();
    }
    match bytes[*index] {
        b'[' => {
            *index += 1;
            format!("{}[]", parse_full_type(bytes, index))
        }
        b'L' => {
            *index += 1;
            let start = *index;
            while *index < bytes.len() && bytes[*index] != b';' {
                *index += 1;
            }
            let name = String::from_utf8_lossy(&bytes[start..*index]).replace('/', ".");
            if bytes.get(*index) == Some(&b';') {
                *index += 1;
            }
            name
        }
        b'B' => {
            *index += 1;
            "byte".to_owned()
        }
        b'C' => {
            *index += 1;
            "char".to_owned()
        }
        b'D' => {
            *index += 1;
            "double".to_owned()
        }
        b'F' => {
            *index += 1;
            "float".to_owned()
        }
        b'I' => {
            *index += 1;
            "int".to_owned()
        }
        b'J' => {
            *index += 1;
            "long".to_owned()
        }
        b'S' => {
            *index += 1;
            "short".to_owned()
        }
        b'Z' => {
            *index += 1;
            "boolean".to_owned()
        }
        b'V' => {
            *index += 1;
            "void".to_owned()
        }
        _ => {
            *index += 1;
            String::new()
        }
    }
}

struct Reader<'a> {
    data: &'a [u8],
    position: usize,
}

impl<'a> Reader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, position: 0 }
    }

    fn u1(&mut self) -> Result<u8> {
        let byte = *self
            .data
            .get(self.position)
            .ok_or_else(|| anyhow::anyhow!("unexpected end of class file"))?;
        self.position += 1;
        Ok(byte)
    }

    fn u2(&mut self) -> Result<u16> {
        let bytes = self.take(2)?;
        Ok(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn u4(&mut self) -> Result<u32> {
        let bytes = self.take(4)?;
        Ok(u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn take(&mut self, count: usize) -> Result<&'a [u8]> {
        let end = self
            .position
            .checked_add(count)
            .ok_or_else(|| anyhow::anyhow!("class file offset overflow"))?;
        let bytes = self
            .data
            .get(self.position..end)
            .ok_or_else(|| anyhow::anyhow!("unexpected end of class file"))?;
        self.position = end;
        Ok(bytes)
    }

    fn skip(&mut self, count: usize) -> Result<()> {
        self.take(count).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn formats_descriptors() {
        assert_eq!(format_field_descriptor("Ljava/lang/String;"), "String");
        assert_eq!(format_field_descriptor("[I"), "int[]");
        assert_eq!(
            format_method_descriptor("(Ljava/lang/String;I)V"),
            ("String, int".to_owned(), "void".to_owned())
        );
        assert_eq!(
            format_method_descriptor("()Ljava/util/List;"),
            (String::new(), "List".to_owned())
        );
    }

    #[test]
    fn parses_signature_type_params() {
        assert_eq!(signature_type_params("Ljava/lang/Object;"), Vec::new());
        assert_eq!(
            signature_type_params("<T::Ljava/lang/Object;>Ljava/lang/Object;"),
            vec![("T".to_owned(), Some("java.lang.Object".to_owned()))]
        );
        assert_eq!(
            signature_type_params("<E:Ljava/lang/Number;>Ljava/lang/Object;"),
            vec![("E".to_owned(), Some("java.lang.Number".to_owned()))]
        );
        assert_eq!(
            signature_type_params("<K:Ljava/lang/Object;V:Ljava/lang/Object;>Ljava/lang/Object;"),
            vec![
                ("K".to_owned(), Some("java.lang.Object".to_owned())),
                ("V".to_owned(), Some("java.lang.Object".to_owned())),
            ]
        );
    }

    #[test]
    fn parses_signature_return_types() {
        assert_eq!(signature_return_text("(TT;)TT;"), Some("T".to_owned()));
        assert_eq!(
            signature_return_text("(I)Ljava/util/List<TT;>;"),
            Some("java.util.List<T>".to_owned())
        );
        assert_eq!(
            signature_return_text("()Ljava/lang/String;"),
            Some("java.lang.String".to_owned())
        );
        assert_eq!(
            signature_return_text("<T:Ljava/lang/Object;>(TT;)Ljava/util/List<TT;>;"),
            Some("java.util.List<T>".to_owned())
        );
        assert_eq!(signature_return_text("(I)V"), Some("void".to_owned()));
    }
}
