//! class 文件解析：常量池、方法与字节码里的类型引用
//!
//! 只解析判定需要的部分，不追求完整还原 class 结构：
//!
//! - 常量池（方法 / 字段 / 类引用要按索引查回名字）
//! - 每个方法的 `Code`：方法体里引用到哪些类型、调用了谁
//! - 类级注解：识别 `@Mod` 入口类
//!
//! 字节码按指令长度逐条走，`tableswitch` / `lookupswitch` / `wide` 单独处理，
//! 否则一条变长指令就会让后续偏移错位，把调用链读成噪声。
//!
//! 解析失败一律返回 `None`：检测器宁可少一条信号，也不能基于半份字节给出
//! 「服务端不可用」的结论。

/// Forge / `NeoForge` / 旧 FML 的入口注解描述符
const MOD_ANNOTATIONS: [&str; 3] = [
    "Lnet/minecraftforge/fml/common/Mod;",
    "Lnet/neoforged/fml/common/Mod;",
    "Lcpw/mods/fml/common/Mod;",
];

/// 常量池能解析出的内容
///
/// 其余常量（数值、String、Dynamic 等）与判定无关，统一记 [`Cp::Other`]，
/// 但**必须**按各自长度跳过，否则后面全部错位。
enum Cp {
    Utf8(String),
    Class(u16),
    NameType { name: u16, descriptor: u16 },
    Member { class: u16, name_type: u16 },
    Other,
}

/// 一个 class 文件
#[derive(Debug, Clone)]
pub(crate) struct ClassFile {
    /// 内部名（`com/example/Foo`）
    pub(crate) name: String,
    /// 类上是否带 `@Mod`（Forge / `NeoForge` / 旧 FML）
    pub(crate) mod_entry: bool,
    /// `@Mod` 的 dist 限制是否只允许客户端
    ///
    /// `NeoForge` 的 `@Mod(dist = Dist.CLIENT)` 入口在专用服务端不被装载；
    /// 判定入口链时必须跳过它，否则会把「只在客户端存在的入口」误算成
    /// 「服务端会执行」。
    pub(crate) mod_client_only: bool,
    pub(crate) methods: Vec<Method>,
}

/// 方法体里的引用
#[derive(Debug, Clone)]
pub(crate) struct Method {
    pub(crate) name: String,
    pub(crate) descriptor: String,
    /// 方法体直接引用到的类型（内部名）
    ///
    /// 来源有三处：`new` / `checkcast` / `instanceof` 等指令的类常量，成员
    /// 引用的属主类，以及成员描述符里出现的类型。描述符也要收——实测
    /// `ReplayMod` 的 `invokestatic MCVer.getMinecraft()Lnet/minecraft/client/
    /// Minecraft;` 光看属主是 `MCVer`，客户端类型只写在返回类型里。
    pub(crate) class_refs: Vec<String>,
    /// 方法体发起的调用（属主 / 方法名 / 描述符）
    ///
    /// 字段读写也收在这里：JVM 的字段与方法是同一种成员引用常量，判定只看
    /// 名字对得上就够了。
    pub(crate) calls: Vec<Call>,
    /// 方法体写入的静态字段（`putstatic`）
    ///
    /// 单独记录是为了识别「静态布尔字段就是客 / 服判断」这种写法：实测
    /// Cupboard 的 `<clinit>` 把 `IS_CLIENT_OR_INTEGRATED` 赋成
    /// `FMLEnvironment.dist == Dist.CLIENT`，配置加载里读这个字段来跳过客户端
    /// 兼容层。只看读点分不清它是不是 dist 判断，得看写点。
    pub(crate) field_writes: Vec<Call>,
}

/// 一条方法调用
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Call {
    pub(crate) owner: String,
    pub(crate) name: String,
    pub(crate) descriptor: String,
}

impl ClassFile {
    /// 静态初始化与构造方法：类装载、实例化时执行的入口
    pub(crate) fn init_roots(&self) -> impl Iterator<Item = &Method> {
        self.methods
            .iter()
            .filter(|m| matches!(m.name.as_str(), "<clinit>" | "<init>"))
    }
}

/// 解析一个 class；结构损坏时返回 `None`
#[must_use]
pub(crate) fn parse(bytes: &[u8]) -> Option<ClassFile> {
    let mut reader = Reader::new(bytes);
    if reader.u4()? != 0xCA_FE_BA_BE {
        return None;
    }
    reader.u2()?; // minor
    reader.u2()?; // major
    let cp = parse_constant_pool(&mut reader)?;

    reader.u2()?; // access_flags
    let this_class = reader.u2()?;
    reader.u2()?; // super_class
    let interfaces = reader.u2()?;
    reader.skip(usize::from(interfaces) * 2)?;

    skip_members(&mut reader)?;

    let name = class_name(&cp, this_class)?.to_owned();
    let methods = parse_methods(&mut reader, &cp)?;
    let annotations = parse_class_annotations(&mut reader, &cp);
    let mod_entry = annotations
        .types
        .iter()
        .any(|annotation| MOD_ANNOTATIONS.contains(&annotation.as_str()));
    // `@Mod(dist = Dist.CLIENT)` 的入口在专用服务端上不会被装载，它的静态
    // 初始化自然不会执行。实测 `create-1.21.1`：主入口 `Create` 无 dist 限制，
    // 而 `CreateClient` 标注 `dist = CLIENT`，两者都带 `@Mod`；把后者当入口
    // 会把它引用的 `Minecraft` 判成「服务端必崩」，误杀整个 Create。
    let mod_client_only = !annotations.mod_dists.is_empty()
        && annotations.mod_dists.iter().all(|dist| dist == "CLIENT");

    Some(ClassFile {
        name,
        mod_entry,
        mod_client_only,
        methods,
    })
}

/// 跳过字段区（判定不读字段）
fn skip_members(reader: &mut Reader<'_>) -> Option<()> {
    let count = reader.u2()?;
    for _ in 0..count {
        reader.u2()?; // access
        reader.u2()?; // name
        reader.u2()?; // descriptor
        skip_attributes(reader)?;
    }
    Some(())
}

/// 跳过一段属性表
fn skip_attributes(reader: &mut Reader<'_>) -> Option<()> {
    let count = reader.u2()?;
    for _ in 0..count {
        reader.u2()?; // name
        let len = usize::try_from(reader.u4()?).ok()?;
        reader.skip(len)?;
    }
    Some(())
}

fn parse_constant_pool(reader: &mut Reader<'_>) -> Option<Vec<Option<Cp>>> {
    let count = usize::from(reader.u2()?);
    let mut cp = Vec::new();
    cp.resize_with(count, || None);
    let mut i = 1;
    while i < count {
        let tag = reader.u1()?;
        cp[i] = Some(match tag {
            1 => {
                let len = usize::from(reader.u2()?);
                let raw = reader.take(len)?;
                Cp::Utf8(String::from_utf8_lossy(raw).into_owned())
            }
            3 | 4 => {
                reader.skip(4)?;
                Cp::Other
            }
            // long / double 占两个槽位
            5 | 6 => {
                reader.skip(8)?;
                i += 1;
                Cp::Other
            }
            7 => Cp::Class(reader.u2()?),
            8 | 16 | 19 | 20 => {
                reader.u2()?;
                Cp::Other
            }
            9..=11 => Cp::Member {
                class: reader.u2()?,
                name_type: reader.u2()?,
            },
            12 => Cp::NameType {
                name: reader.u2()?,
                descriptor: reader.u2()?,
            },
            15 => {
                reader.u1()?;
                reader.u2()?;
                Cp::Other
            }
            17 | 18 => {
                reader.u2()?;
                reader.u2()?;
                Cp::Other
            }
            _ => return None,
        });
        i += 1;
    }
    Some(cp)
}

fn utf8(cp: &[Option<Cp>], index: u16) -> Option<&str> {
    match cp.get(usize::from(index))?.as_ref()? {
        Cp::Utf8(text) => Some(text),
        _ => None,
    }
}

fn class_name(cp: &[Option<Cp>], index: u16) -> Option<&str> {
    match cp.get(usize::from(index))?.as_ref()? {
        Cp::Class(name) => utf8(cp, *name),
        _ => None,
    }
}

/// 成员引用 → `(属主类, 方法名, 描述符)`
fn member(cp: &[Option<Cp>], index: u16) -> Option<(&str, &str, &str)> {
    let Cp::Member { class, name_type } = cp.get(usize::from(index))?.as_ref()? else {
        return None;
    };
    let Cp::NameType { name, descriptor } = cp.get(usize::from(*name_type))?.as_ref()? else {
        return None;
    };
    Some((
        class_name(cp, *class)?,
        utf8(cp, *name)?,
        utf8(cp, *descriptor)?,
    ))
}

fn parse_methods(reader: &mut Reader<'_>, cp: &[Option<Cp>]) -> Option<Vec<Method>> {
    let count = reader.u2()?;
    let mut methods = Vec::new();
    for _ in 0..count {
        reader.u2()?; // access
        let name = utf8(cp, reader.u2()?).unwrap_or_default().to_owned();
        let descriptor = utf8(cp, reader.u2()?).unwrap_or_default().to_owned();
        let mut code = None;
        let attributes = reader.u2()?;
        for _ in 0..attributes {
            let attribute_name = utf8(cp, reader.u2()?);
            let len = usize::try_from(reader.u4()?).ok()?;
            let body = reader.take(len)?;
            if attribute_name == Some("Code") {
                code = Some(body);
            }
        }
        let mut method = Method {
            name,
            descriptor,
            class_refs: Vec::new(),
            calls: Vec::new(),
            field_writes: Vec::new(),
        };
        if let Some(body) = code {
            scan_code_attribute(body, cp, &mut method);
        }
        methods.push(method);
    }
    Some(methods)
}

/// 读 `Code` 属性里的指令序列
///
/// 异常表与子属性在指令之后，判定不需要；指令读坏了就丢掉整个方法体，
/// 保留方法本身（其签名仍可能被调用方引用）。
fn scan_code_attribute(body: &[u8], cp: &[Option<Cp>], method: &mut Method) {
    let mut reader = Reader::new(body);
    let Some(code) = (|| {
        reader.u2()?; // max_stack
        reader.u2()?; // max_locals
        let len = usize::try_from(reader.u4()?).ok()?;
        reader.take(len)
    })() else {
        return;
    };
    scan_code(code, cp, method);
}

fn scan_code(code: &[u8], cp: &[Option<Cp>], method: &mut Method) {
    let mut pos = 0;
    while pos < code.len() {
        let Some(len) = instruction_len(code, pos) else {
            break;
        };
        if let Some(index) = constant_operand(code, pos, len) {
            record_constant(cp, index, code[pos], method);
        }
        pos += len;
    }
}

/// 取引用常量池的指令的操作数（无则 `None`）
fn constant_operand(code: &[u8], pos: usize, len: usize) -> Option<u16> {
    let op = *code.get(pos)?;
    match op {
        // ldc 的索引是一个字节
        0x12 if len >= 2 => code.get(pos + 1).map(|b| u16::from(*b)),
        // ldc_w / ldc2_w / 字段与调用指令 / new / checkcast 等：两字节索引
        0x13 | 0x14 | 0xb2..=0xb9 | 0xbb | 0xbd | 0xc0 | 0xc1 | 0xc5 if len >= 3 => {
            Some(u16::from_be_bytes([
                *code.get(pos + 1)?,
                *code.get(pos + 2)?,
            ]))
        }
        _ => None,
    }
}

fn record_constant(cp: &[Option<Cp>], index: u16, opcode: u8, method: &mut Method) {
    match cp.get(usize::from(index)).and_then(Option::as_ref) {
        Some(Cp::Class(_)) => {
            if let Some(name) = class_name(cp, index) {
                method.class_refs.push(name.to_owned());
            }
        }
        Some(Cp::Member { .. }) => {
            if let Some((owner, name, descriptor)) = member(cp, index) {
                let reference = Call {
                    owner: owner.to_owned(),
                    name: name.to_owned(),
                    descriptor: descriptor.to_owned(),
                };
                method.class_refs.push(owner.to_owned());
                push_descriptor_types(descriptor, &mut method.class_refs);
                if opcode == 0xb3 {
                    method.field_writes.push(reference.clone());
                }
                method.calls.push(reference);
            }
        }
        _ => {}
    }
}

/// 把描述符里的对象类型追加进引用清单
///
/// 描述符是 ASCII：`(Lnet/minecraft/client/Minecraft;I)V` → 取 `L...;` 中间。
fn push_descriptor_types(descriptor: &str, out: &mut Vec<String>) {
    let bytes = descriptor.as_bytes();
    let mut start = 0;
    while start < bytes.len() {
        if bytes[start] == b'L'
            && let Some(offset) = descriptor[start + 1..].find(';')
        {
            let end = start + 1 + offset;
            out.push(descriptor[start + 1..end].to_owned());
            start = end + 1;
            continue;
        }
        start += 1;
    }
}

/// 单条指令占多少字节；无法识别的指令返回 `None`
fn instruction_len(code: &[u8], pos: usize) -> Option<usize> {
    let op = *code.get(pos)?;
    match op {
        // tableswitch / lookupswitch 按 4 字节对齐补位后跟跳转表
        0xaa => {
            let base = (pos + 4) & !3;
            let low = read_i32(code, base + 4)?;
            let high = read_i32(code, base + 8)?;
            if high < low {
                return None;
            }
            // 跳转偏移的个数是 high - low + 1；default 那 4 字节已包含在
            // base + 12 里，不能重复计一次
            let offsets = usize::try_from(i64::from(high) - i64::from(low) + 1).ok()?;
            Some(base + 12 + offsets * 4 - pos)
        }
        0xab => {
            let base = (pos + 4) & !3;
            let pairs = read_i32(code, base + 4)?;
            if pairs < 0 {
                return None;
            }
            Some(base + 8 + usize::try_from(pairs).ok()? * 8 - pos)
        }
        // wide 后面跟一条被加宽的指令
        0xc4 => match code.get(pos + 1)? {
            0x84 => Some(6),
            _ => Some(4),
        },
        _ => simple_instruction_len(op),
    }
}

fn read_i32(code: &[u8], at: usize) -> Option<i32> {
    let bytes = code.get(at..at + 4)?;
    Some(i32::from_be_bytes(bytes.try_into().ok()?))
}

/// 定长指令的长度表
fn simple_instruction_len(op: u8) -> Option<usize> {
    Some(match op {
        0x00..=0x0f
        | 0x1a..=0x35
        | 0x3b..=0x83
        | 0x85..=0x98
        | 0xac..=0xb1
        | 0xbe
        | 0xbf
        | 0xc2
        | 0xc3
        | 0xca
        | 0xfe
        | 0xff => 1,
        0x10 | 0x12 | 0x15..=0x19 | 0x36..=0x3a | 0xa9 | 0xbc => 2,
        0x11
        | 0x13
        | 0x14
        | 0x84
        | 0x99..=0xa8
        | 0xb2..=0xb8
        | 0xbb
        | 0xbd
        | 0xc0
        | 0xc1
        | 0xc6
        | 0xc7 => 3,
        0xc5 => 4,
        0xb9 | 0xba | 0xc8 | 0xc9 => 5,
        _ => return None,
    })
}

/// 类级注解解析结果
#[derive(Debug, Default)]
struct ClassAnnotations {
    /// 注解类型描述符
    types: Vec<String>,
    /// `@Mod` 上的 `dist` 元素取值（`Dist` 枚举常量名，如 `CLIENT`）
    mod_dists: Vec<String>,
}

/// 类级注解的类型描述符与 `@Mod` 的 dist 限制
fn parse_class_annotations(reader: &mut Reader<'_>, cp: &[Option<Cp>]) -> ClassAnnotations {
    let mut out = ClassAnnotations::default();
    let Some(count) = reader.u2() else {
        return out;
    };
    for _ in 0..count {
        let Some(name) = reader.u2() else {
            return out;
        };
        let Some(len) = reader.u4().and_then(|n| usize::try_from(n).ok()) else {
            return out;
        };
        let Some(body) = reader.take(len) else {
            return out;
        };
        if matches!(
            utf8(cp, name),
            Some("RuntimeVisibleAnnotations" | "RuntimeInvisibleAnnotations")
        ) {
            collect_annotation_types(body, cp, &mut out);
        }
    }
    out
}

fn collect_annotation_types(body: &[u8], cp: &[Option<Cp>], out: &mut ClassAnnotations) {
    let mut reader = Reader::new(body);
    let Some(count) = reader.u2() else {
        return;
    };
    for _ in 0..count {
        let Some(type_index) = reader.u2() else {
            return;
        };
        let annotation_type = utf8(cp, type_index).map(ToOwned::to_owned);
        if let Some(text) = &annotation_type {
            out.types.push(text.clone());
        }
        let Some(pairs) = reader.u2() else {
            return;
        };
        for _ in 0..pairs {
            let Some(element_name) = reader.u2() else {
                return;
            };
            let is_mod = annotation_type
                .as_deref()
                .is_some_and(|text| MOD_ANNOTATIONS.contains(&text));
            let name_is_dist = utf8(cp, element_name) == Some("dist");
            if !(is_mod && name_is_dist) {
                if skip_element_value(&mut reader, 0).is_none() {
                    return;
                }
                continue;
            }
            match read_dist_element(&mut reader, cp) {
                Some(dists) => out.mod_dists.extend(dists),
                None => return,
            }
        }
    }
}

/// 读 `@Mod(dist = ...)` 的元素值
///
/// `dist` 是数组：`dist = {Dist.CLIENT, Dist.DEDICATED_SERVER}`，每个元素是
/// 枚举常量（tag `e`，两个索引分别指向类型描述符与常量名）。
fn read_dist_element(reader: &mut Reader<'_>, cp: &[Option<Cp>]) -> Option<Vec<String>> {
    let mut out = Vec::new();
    match reader.u1()? {
        b'e' => {
            reader.u2()?; // 枚举类型描述符
            let name = utf8(cp, reader.u2()?)?;
            out.push(name.to_owned());
        }
        b'[' => {
            let count = reader.u2()?;
            for _ in 0..count {
                let tag = reader.u1()?;
                if tag != b'e' {
                    return None;
                }
                reader.u2()?;
                let name = utf8(cp, reader.u2()?)?;
                out.push(name.to_owned());
            }
        }
        _ => return None,
    }
    Some(out)
}

/// 跳过注解元素值
fn skip_element_value(reader: &mut Reader<'_>, depth: usize) -> Option<()> {
    if depth > 32 {
        return None;
    }
    match reader.u1()? {
        // `s` 是 String 常量索引；`c` 是 class 常量索引
        b'B' | b'C' | b'D' | b'F' | b'I' | b'J' | b'S' | b'Z' | b'c' | b's' => reader.skip(2),
        b'e' => reader.skip(4),
        b'@' => skip_annotation(reader, depth + 1),
        b'[' => {
            let count = reader.u2()?;
            for _ in 0..count {
                skip_element_value(reader, depth + 1)?;
            }
            Some(())
        }
        _ => None,
    }
}

fn skip_annotation(reader: &mut Reader<'_>, depth: usize) -> Option<()> {
    reader.u2()?; // type
    let pairs = reader.u2()?;
    for _ in 0..pairs {
        reader.u2()?;
        skip_element_value(reader, depth)?;
    }
    Some(())
}

/// 大端读取器，越界一律 `None`
struct Reader<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    const fn new(bytes: &'a [u8]) -> Self {
        Self { bytes, pos: 0 }
    }

    fn u1(&mut self) -> Option<u8> {
        let value = *self.bytes.get(self.pos)?;
        self.pos += 1;
        Some(value)
    }

    fn u2(&mut self) -> Option<u16> {
        let bytes = self.take(2)?;
        Some(u16::from_be_bytes(bytes.try_into().ok()?))
    }

    fn u4(&mut self) -> Option<u32> {
        let bytes = self.take(4)?;
        Some(u32::from_be_bytes(bytes.try_into().ok()?))
    }

    fn take(&mut self, len: usize) -> Option<&'a [u8]> {
        let end = self.pos.checked_add(len)?;
        let slice = self.bytes.get(self.pos..end)?;
        self.pos = end;
        Some(slice)
    }

    fn skip(&mut self, len: usize) -> Option<()> {
        self.take(len).map(|_| ())
    }
}

#[cfg(test)]
mod tests {
    // 测试用 expect 是刻意的：解析器坏了必须直接报出来
    #![allow(clippy::expect_used)]
    #![allow(clippy::panic)]

    use super::{Call, parse};

    /// 最小 class 构造器：只写判定关心的常量池、方法、注解
    struct Builder {
        pool: Vec<Pool>,
    }

    enum Pool {
        Utf8(String),
        Class(u16),
        NameType(u16, u16),
        Member(u16, u16),
    }

    impl Builder {
        fn new() -> Self {
            let mut builder = Self { pool: Vec::new() };
            // 槽位 0 恒空
            builder.pool.push(Pool::Utf8(String::new()));
            builder
        }

        fn utf8(&mut self, text: &str) -> u16 {
            self.pool.push(Pool::Utf8(text.to_owned()));
            u16::try_from(self.pool.len() - 1).expect("测试 class 的常量池不会超限")
        }

        fn class(&mut self, name: &str) -> u16 {
            let name = self.utf8(name);
            self.pool.push(Pool::Class(name));
            u16::try_from(self.pool.len() - 1).expect("测试 class 的常量池不会超限")
        }

        fn method_ref(&mut self, owner: &str, name: &str, descriptor: &str) -> u16 {
            let class = self.class(owner);
            let name = self.utf8(name);
            let descriptor = self.utf8(descriptor);
            self.pool.push(Pool::NameType(name, descriptor));
            let name_type =
                u16::try_from(self.pool.len() - 1).expect("测试 class 的常量池不会超限");
            self.pool.push(Pool::Member(class, name_type));
            u16::try_from(self.pool.len() - 1).expect("测试 class 的常量池不会超限")
        }

        fn build(self, class_name: &str, annotations: &[&str], methods: &[Method]) -> Vec<u8> {
            self.build_with_dist(class_name, annotations, None, methods)
        }

        /// 构造带 `@Mod(dist = {…})` 的 class
        ///
        /// `dist` 只写到第一个注解上；测试只需要覆盖「入口被 dist 限制」这条
        /// 路径，不必模拟多个注解各带元素值。
        fn build_with_dist(
            self,
            class_name: &str,
            annotations: &[&str],
            dist: Option<&[&str]>,
            methods: &[Method],
        ) -> Vec<u8> {
            let mut pool = self;
            let this = pool.class(class_name);
            // 属性名与注解描述符必须在写常量池计数之前全部登记好：
            // 计数写定之后再往池里加项，字节流会和计数对不上。
            let code_attr = pool.utf8("Code");
            let annotations_attr = pool.utf8("RuntimeVisibleAnnotations");
            let annotation_types = annotations
                .iter()
                .map(|annotation| pool.utf8(annotation))
                .collect::<Vec<_>>();
            let dist_indices = pool.dist_indices(dist);

            let mut out = vec![0xCA, 0xFE, 0xBA, 0xBE, 0x00, 0x00, 0x00, 0x3D];
            let count = u16::try_from(pool.pool.len()).expect("测试 class 的常量池不会超限");
            out.extend_from_slice(&count.to_be_bytes());
            for entry in pool.pool.iter().skip(1) {
                write_pool_entry(&mut out, entry);
            }
            write_class_header(&mut out, this, methods, code_attr);
            write_annotations(
                &mut out,
                &annotation_types,
                annotations_attr,
                dist_indices.as_ref(),
            );
            out
        }

        /// `dist` 元素需要的常量池项：元素名、枚举类型、各常量名
        fn dist_indices(&mut self, dist: Option<&[&str]>) -> Option<(u16, u16, Vec<u16>)> {
            let dists = dist?;
            let element = self.utf8("dist");
            let enum_type = self.utf8("Lnet/neoforged/api/distmarker/Dist;");
            let names = dists.iter().map(|name| self.utf8(name)).collect();
            Some((element, enum_type, names))
        }
    }

    /// 写一个常量池项（测试池不含 long / double，无跳号）
    fn write_pool_entry(out: &mut Vec<u8>, entry: &Pool) {
        match entry {
            Pool::Utf8(text) => {
                out.push(1);
                let len = u16::try_from(text.len()).expect("测试 class 的常量池不会超限");
                out.extend_from_slice(&len.to_be_bytes());
                out.extend_from_slice(text.as_bytes());
            }
            Pool::Class(name) => {
                out.push(7);
                out.extend_from_slice(&name.to_be_bytes());
            }
            Pool::NameType(name, descriptor) => {
                out.push(12);
                out.extend_from_slice(&name.to_be_bytes());
                out.extend_from_slice(&descriptor.to_be_bytes());
            }
            Pool::Member(class, name_type) => {
                out.push(10);
                out.extend_from_slice(&class.to_be_bytes());
                out.extend_from_slice(&name_type.to_be_bytes());
            }
        }
    }

    /// 写类头、字段表与方法表
    fn write_class_header(out: &mut Vec<u8>, this: u16, methods: &[Method], code_attr: u16) {
        out.extend_from_slice(&0x0021u16.to_be_bytes()); // public
        out.extend_from_slice(&this.to_be_bytes());
        out.extend_from_slice(&0x0000u16.to_be_bytes()); // super
        out.extend_from_slice(&0x0000u16.to_be_bytes()); // interfaces
        out.extend_from_slice(&0x0000u16.to_be_bytes()); // fields
        out.extend_from_slice(
            &u16::try_from(methods.len())
                .expect("测试 class 的方法数不会超限")
                .to_be_bytes(),
        );
        for method in methods {
            out.extend_from_slice(&0x0009u16.to_be_bytes()); // static
            out.extend_from_slice(&method.name.to_be_bytes());
            out.extend_from_slice(&method.descriptor.to_be_bytes());
            let has_code = !method.code.is_empty();
            out.extend_from_slice(&u16::from(has_code).to_be_bytes());
            if has_code {
                out.extend_from_slice(&code_attr.to_be_bytes());
                let mut body = Vec::new();
                body.extend_from_slice(&0u16.to_be_bytes()); // max_stack
                body.extend_from_slice(&0u16.to_be_bytes()); // max_locals
                body.extend_from_slice(
                    &u32::try_from(method.code.len())
                        .expect("测试 class 的方法体不会超限")
                        .to_be_bytes(),
                );
                body.extend_from_slice(&method.code);
                body.extend_from_slice(&0u16.to_be_bytes()); // exception table
                body.extend_from_slice(&0u16.to_be_bytes()); // attributes
                out.extend_from_slice(
                    &u32::try_from(body.len())
                        .expect("测试 class 的属性不会超限")
                        .to_be_bytes(),
                );
                out.extend_from_slice(&body);
            }
        }
    }

    /// 写类级注解属性；`dist` 只挂在第一个注解上
    fn write_annotations(
        out: &mut Vec<u8>,
        annotation_types: &[u16],
        annotations_attr: u16,
        dist: Option<&(u16, u16, Vec<u16>)>,
    ) {
        let has_annotations = !annotation_types.is_empty();
        out.extend_from_slice(&u16::from(has_annotations).to_be_bytes());
        if !has_annotations {
            return;
        }
        out.extend_from_slice(&annotations_attr.to_be_bytes());
        let mut body = Vec::new();
        body.extend_from_slice(
            &u16::try_from(annotation_types.len())
                .expect("测试 class 的注解数不会超限")
                .to_be_bytes(),
        );
        for (position, annotation) in annotation_types.iter().enumerate() {
            body.extend_from_slice(&annotation.to_be_bytes());
            match dist.filter(|_| position == 0) {
                Some((element, enum_type, names)) => {
                    body.extend_from_slice(&1u16.to_be_bytes());
                    body.extend_from_slice(&element.to_be_bytes());
                    body.push(b'[');
                    body.extend_from_slice(
                        &u16::try_from(names.len())
                            .expect("测试 class 的 dist 元素数不会超限")
                            .to_be_bytes(),
                    );
                    for name in names {
                        body.push(b'e');
                        body.extend_from_slice(&enum_type.to_be_bytes());
                        body.extend_from_slice(&name.to_be_bytes());
                    }
                }
                None => body.extend_from_slice(&0u16.to_be_bytes()),
            }
        }
        out.extend_from_slice(
            &u32::try_from(body.len())
                .expect("测试 class 的属性不会超限")
                .to_be_bytes(),
        );
        out.extend_from_slice(&body);
    }

    /// 测试方法：常量池里先建好 name / descriptor，再填指令
    struct Method {
        name: u16,
        descriptor: u16,
        code: Vec<u8>,
    }

    impl Method {
        fn new(builder: &mut Builder, name: &str, descriptor: &str, code: Vec<u8>) -> Self {
            Self {
                name: builder.utf8(name),
                descriptor: builder.utf8(descriptor),
                code,
            }
        }
    }

    fn invokestatic(index: u16) -> Vec<u8> {
        let mut out = vec![0xb8];
        out.extend_from_slice(&index.to_be_bytes());
        out
    }

    #[test]
    fn reads_mod_annotation() {
        let mut builder = Builder::new();
        let method = Method::new(&mut builder, "run", "()V", vec![0xb1]);
        let bytes = builder.build(
            "com/example/Entry",
            &["Lnet/minecraftforge/fml/common/Mod;"],
            &[method],
        );
        let class = parse(&bytes).expect("合法 class");
        assert_eq!(class.name, "com/example/Entry");
        assert!(class.mod_entry);
    }

    #[test]
    fn plain_class_is_not_a_mod_entry() {
        let mut builder = Builder::new();
        let method = Method::new(&mut builder, "run", "()V", vec![0xb1]);
        let bytes = builder.build("com/example/Helper", &[], &[method]);
        assert!(!parse(&bytes).expect("合法 class").mod_entry);
    }

    /// `@Mod(dist = Dist.CLIENT)`：入口只在客户端装载，必须标记出来
    #[test]
    fn reads_mod_client_dist() {
        let mut builder = Builder::new();
        let method = Method::new(&mut builder, "run", "()V", vec![0xb1]);
        let bytes = builder.build_with_dist(
            "com/example/CreateClient",
            &["Lnet/neoforged/fml/common/Mod;"],
            Some(&["CLIENT"]),
            &[method],
        );
        let class = parse(&bytes).expect("合法 class");
        assert!(class.mod_entry);
        assert!(class.mod_client_only);
    }

    /// 双端 dist（`CLIENT` + `DEDICATED_SERVER`）不算仅客户端
    #[test]
    fn dual_dist_is_not_client_only() {
        let mut builder = Builder::new();
        let method = Method::new(&mut builder, "run", "()V", vec![0xb1]);
        let bytes = builder.build_with_dist(
            "com/example/Entry",
            &["Lnet/neoforged/fml/common/Mod;"],
            Some(&["CLIENT", "DEDICATED_SERVER"]),
            &[method],
        );
        let class = parse(&bytes).expect("合法 class");
        assert!(class.mod_entry);
        assert!(!class.mod_client_only);
    }

    /// 没有 dist 元素就是无限制
    #[test]
    fn entry_without_dist_is_not_client_only() {
        let mut builder = Builder::new();
        let method = Method::new(&mut builder, "run", "()V", vec![0xb1]);
        let bytes = builder.build(
            "com/example/Entry",
            &["Lnet/minecraftforge/fml/common/Mod;"],
            &[method],
        );
        assert!(!parse(&bytes).expect("合法 class").mod_client_only);
    }

    /// 返回类型里的客户端类也要收：`ReplayMod` 的崩溃就藏在返回类型
    #[test]
    fn records_descriptor_types_and_calls() {
        let mut builder = Builder::new();
        let target = builder.method_ref(
            "com/example/MCVer",
            "getMinecraft",
            "()Lnet/minecraft/client/Minecraft;",
        );
        let method = Method::new(
            &mut builder,
            "<clinit>",
            "()V",
            [invokestatic(target), vec![0xb1]].concat(),
        );
        let bytes = builder.build("com/example/Mod", &[], &[method]);
        let class = parse(&bytes).expect("合法 class");
        let init = class.init_roots().next().expect("clinit");
        assert!(
            init.class_refs
                .iter()
                .any(|name| name == "net/minecraft/client/Minecraft"),
            "描述符里的返回类型应当进引用清单：{:?}",
            init.class_refs
        );
        assert_eq!(
            init.calls,
            vec![Call {
                owner: "com/example/MCVer".to_owned(),
                name: "getMinecraft".to_owned(),
                descriptor: "()Lnet/minecraft/client/Minecraft;".to_owned(),
            }]
        );
    }

    /// tableswitch 是变长的：对齐算错会让后面的调用读不到
    #[test]
    fn variable_length_switch_does_not_desync() {
        let mut builder = Builder::new();
        let target = builder.method_ref("com/example/Target", "touch", "()V");
        let mut code = vec![0xaa]; // tableswitch
        code.extend_from_slice(&[0, 0, 0]); // 补齐到 4 字节边界
        code.extend_from_slice(&0i32.to_be_bytes()); // default
        code.extend_from_slice(&0i32.to_be_bytes()); // low
        code.extend_from_slice(&1i32.to_be_bytes()); // high
        code.extend_from_slice(&0i32.to_be_bytes()); // branch 0
        code.extend_from_slice(&0i32.to_be_bytes()); // branch 1
        code.extend(invokestatic(target));
        code.push(0xb1);
        let method = Method::new(&mut builder, "<clinit>", "()V", code);
        let bytes = builder.build("com/example/Mod", &[], &[method]);
        let class = parse(&bytes).expect("合法 class");
        let init = class.init_roots().next().expect("clinit");
        assert_eq!(init.calls.len(), 1, "switch 后的调用必须被读到");
        assert_eq!(init.calls[0].name, "touch");
    }

    #[test]
    fn truncated_class_is_rejected() {
        assert!(parse(&[0xCA, 0xFE, 0xBA, 0xBE, 0x00]).is_none());
        assert!(parse(b"not a class").is_none());
    }
}
