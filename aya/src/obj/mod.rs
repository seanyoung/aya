pub(crate) mod btf;
mod relocation;

use object::{
    read::{Object as ElfObject, ObjectSection, Section as ObjSection},
    Endianness, ObjectSymbol, ObjectSymbolTable, RelocationTarget, SectionIndex,
};
use std::{
    collections::HashMap,
    convert::{TryFrom, TryInto},
    ffi::{CStr, CString},
    mem, ptr,
    str::FromStr,
};
use thiserror::Error;

use relocation::*;

use crate::{
    bpf_map_def,
    generated::{bpf_insn, bpf_map_type::BPF_MAP_TYPE_ARRAY},
    obj::btf::{Btf, BtfError, BtfExt},
    BpfError,
};

const KERNEL_VERSION_ANY: u32 = 0xFFFF_FFFE;

#[derive(Clone)]
pub struct Object {
    pub(crate) endianness: Endianness,
    pub license: CString,
    pub kernel_version: KernelVersion,
    pub btf: Option<Btf>,
    pub btf_ext: Option<BtfExt>,
    pub(crate) maps: HashMap<String, Map>,
    pub(crate) programs: HashMap<String, Program>,
    pub(crate) functions: HashMap<u64, Function>,
    pub(crate) relocations: HashMap<SectionIndex, HashMap<u64, Relocation>>,
    pub(crate) symbols_by_index: HashMap<usize, Symbol>,
}

#[derive(Debug, Clone)]
pub struct Map {
    pub(crate) name: String,
    pub(crate) def: bpf_map_def,
    pub(crate) section_index: usize,
    pub(crate) data: Vec<u8>,
}

#[derive(Debug, Clone)]
pub(crate) struct Program {
    pub(crate) license: CString,
    pub(crate) kernel_version: KernelVersion,
    pub(crate) kind: ProgramKind,
    pub(crate) function: Function,
}

#[derive(Debug, Clone)]
pub(crate) struct Function {
    pub(crate) address: u64,
    pub(crate) name: String,
    pub(crate) section_index: SectionIndex,
    pub(crate) section_offset: usize,
    pub(crate) instructions: Vec<bpf_insn>,
}

#[derive(Debug, Copy, Clone)]
pub enum ProgramKind {
    KProbe,
    KRetProbe,
    UProbe,
    URetProbe,
    TracePoint,
    SocketFilter,
    Xdp,
    SkMsg,
    SkSkbStreamParser,
    SkSkbStreamVerdict,
    SockOps,
    SchedClassifier,
    CgroupSkbIngress,
    CgroupSkbEgress,
}

impl FromStr for ProgramKind {
    type Err = ParseError;

    fn from_str(kind: &str) -> Result<ProgramKind, ParseError> {
        use ProgramKind::*;
        Ok(match kind {
            "kprobe" => KProbe,
            "kretprobe" => KRetProbe,
            "uprobe" => UProbe,
            "uretprobe" => URetProbe,
            "xdp" => Xdp,
            "trace_point" => TracePoint,
            "socket_filter" => SocketFilter,
            "sk_msg" => SkMsg,
            "sk_skb/stream_parser" => SkSkbStreamParser,
            "sk_skb/stream_verdict" => SkSkbStreamVerdict,
            "sockops" => SockOps,
            "classifier" => SchedClassifier,
            "cgroup_skb/ingress" => CgroupSkbIngress,
            "cgroup_skb/egress" => CgroupSkbEgress,
            _ => {
                return Err(ParseError::InvalidProgramKind {
                    kind: kind.to_string(),
                })
            }
        })
    }
}

impl Object {
    pub(crate) fn parse(data: &[u8]) -> Result<Object, BpfError> {
        let obj = object::read::File::parse(data).map_err(|e| ParseError::ElfError(e))?;
        let endianness = obj.endianness();

        let license = if let Some(section) = obj.section_by_name("license") {
            parse_license(Section::try_from(&section)?.data)?
        } else {
            CString::new("GPL").unwrap()
        };

        let kernel_version = if let Some(section) = obj.section_by_name("version") {
            parse_version(Section::try_from(&section)?.data, endianness)?
        } else {
            KernelVersion::Any
        };

        let mut bpf_obj = Object::new(endianness, license, kernel_version);

        if let Some(symbol_table) = obj.symbol_table() {
            for symbol in symbol_table.symbols() {
                let sym = Symbol {
                    index: symbol.index().0,
                    name: symbol.name().ok().map(String::from),
                    section_index: symbol.section().index(),
                    address: symbol.address(),
                    size: symbol.size(),
                    is_definition: symbol.is_definition(),
                };
                bpf_obj
                    .symbols_by_index
                    .insert(symbol.index().0, sym.clone());
            }
        }

        for s in obj.sections() {
            bpf_obj.parse_section(Section::try_from(&s)?)?;
        }

        return Ok(bpf_obj);
    }

    fn new(endianness: Endianness, license: CString, kernel_version: KernelVersion) -> Object {
        Object {
            endianness: endianness.into(),
            license,
            kernel_version,
            btf: None,
            btf_ext: None,
            maps: HashMap::new(),
            programs: HashMap::new(),
            functions: HashMap::new(),
            relocations: HashMap::new(),
            symbols_by_index: HashMap::new(),
        }
    }

    fn parse_btf(&mut self, section: &Section) -> Result<(), BtfError> {
        self.btf = Some(Btf::parse(section.data, self.endianness)?);

        Ok(())
    }

    fn parse_btf_ext(&mut self, section: &Section) -> Result<(), BtfError> {
        self.btf_ext = Some(BtfExt::parse(section.data, self.endianness)?);
        Ok(())
    }

    fn parse_program(
        &self,
        section: &Section,
        ty: &str,
        name: &str,
    ) -> Result<Program, ParseError> {
        Ok(Program {
            license: self.license.clone(),
            kernel_version: self.kernel_version,
            kind: ProgramKind::from_str(ty)?,
            function: Function {
                name: name.to_owned(),
                address: section.address,
                section_index: section.index,
                section_offset: 0,
                instructions: copy_instructions(section.data)?,
            },
        })
    }

    fn parse_text_section(&mut self, mut section: Section) -> Result<(), ParseError> {
        let mut symbols_by_address = HashMap::new();

        for sym in self.symbols_by_index.values() {
            if sym.is_definition && sym.section_index == Some(section.index) {
                if symbols_by_address.contains_key(&sym.address) {
                    return Err(ParseError::SymbolTableConflict {
                        section_index: section.index.0,
                        address: sym.address,
                    });
                }
                symbols_by_address.insert(sym.address, sym);
            }
        }

        let mut offset = 0;
        while offset < section.data.len() {
            let address = section.address + offset as u64;
            let sym = symbols_by_address
                .get(&address)
                .ok_or(ParseError::UnknownSymbol {
                    section_index: section.index.0,
                    address,
                })?;
            if sym.size == 0 {
                return Err(ParseError::InvalidSymbol {
                    index: sym.index,
                    name: sym.name.clone(),
                });
            }

            self.functions.insert(
                sym.address,
                Function {
                    address,
                    name: sym.name.clone().unwrap(),
                    section_index: section.index,
                    section_offset: offset,
                    instructions: copy_instructions(
                        &section.data[offset..offset + sym.size as usize],
                    )?,
                },
            );

            offset += sym.size as usize;
        }

        if !section.relocations.is_empty() {
            self.relocations.insert(
                section.index,
                section
                    .relocations
                    .drain(..)
                    .map(|rel| (rel.offset, rel))
                    .collect(),
            );
        }

        Ok(())
    }

    fn parse_section(&mut self, mut section: Section) -> Result<(), BpfError> {
        let mut parts = section.name.rsplitn(2, "/").collect::<Vec<_>>();
        parts.reverse();

        if parts.len() == 1 {
            if parts[0] == "xdp"
                || parts[0] == "sk_msg"
                || parts[0] == "sockops"
                || parts[0] == "classifier"
            {
                parts.push(parts[0]);
            }
        }

        match parts.as_slice() {
            &[name]
                if name == ".bss" || name.starts_with(".data") || name.starts_with(".rodata") =>
            {
                self.maps
                    .insert(name.to_string(), parse_map(&section, name)?);
            }
            &[name] if name.starts_with(".text") => self.parse_text_section(section)?,
            &[".BTF"] => self.parse_btf(&section)?,
            &[".BTF.ext"] => self.parse_btf_ext(&section)?,
            &["maps", name] => {
                self.maps
                    .insert(name.to_string(), parse_map(&section, name)?);
            }
            &[ty @ "kprobe", name]
            | &[ty @ "kretprobe", name]
            | &[ty @ "uprobe", name]
            | &[ty @ "uretprobe", name]
            | &[ty @ "socket_filter", name]
            | &[ty @ "xdp", name]
            | &[ty @ "trace_point", name]
            | &[ty @ "sk_msg", name]
            | &[ty @ "sk_skb/stream_parser", name]
            | &[ty @ "sk_skb/stream_verdict", name]
            | &[ty @ "sockops", name]
            | &[ty @ "classifier", name]
            | &[ty @ "cgroup_skb/ingress", name]
            | &[ty @ "cgroup_skb/egress", name]
            | &[ty @ "cgroup/skb", name] => {
                self.programs
                    .insert(name.to_string(), self.parse_program(&section, ty, name)?);
                if !section.relocations.is_empty() {
                    self.relocations.insert(
                        section.index,
                        section
                            .relocations
                            .drain(..)
                            .map(|rel| (rel.offset, rel))
                            .collect(),
                    );
                }
            }

            _ => {}
        }

        Ok(())
    }
}

#[derive(Debug, Clone, Error)]
pub enum ParseError {
    #[error("error parsing ELF data")]
    ElfError(#[from] object::read::Error),

    #[error("invalid license `{data:?}`: missing NULL terminator")]
    MissingLicenseNullTerminator { data: Vec<u8> },

    #[error("invalid license `{data:?}`")]
    InvalidLicense { data: Vec<u8> },

    #[error("invalid kernel version `{data:?}`")]
    InvalidKernelVersion { data: Vec<u8> },

    #[error("error parsing section with index {index}")]
    SectionError {
        index: usize,
        #[source]
        source: object::read::Error,
    },

    #[error("unsupported relocation target")]
    UnsupportedRelocationTarget,

    #[error("invalid program kind `{kind}`")]
    InvalidProgramKind { kind: String },

    #[error("invalid program code")]
    InvalidProgramCode,

    #[error("error parsing map `{name}`")]
    InvalidMapDefinition { name: String },

    #[error("two or more symbols in section `{section_index}` have the same address {address:x}")]
    SymbolTableConflict { section_index: usize, address: u64 },

    #[error("unknown symbol in section `{section_index}` at address {address:x}")]
    UnknownSymbol { section_index: usize, address: u64 },

    #[error("invalid symbol, index `{index}` name: {}", .name.as_ref().unwrap_or(&"[unknown]".into()))]
    InvalidSymbol { index: usize, name: Option<String> },
}

#[derive(Debug)]
struct Section<'a> {
    index: SectionIndex,
    address: u64,
    name: &'a str,
    data: &'a [u8],
    relocations: Vec<Relocation>,
}

impl<'data, 'file, 'a> TryFrom<&'a ObjSection<'data, 'file>> for Section<'a> {
    type Error = ParseError;

    fn try_from(section: &'a ObjSection) -> Result<Section<'a>, ParseError> {
        let index = section.index();
        let map_err = |source| ParseError::SectionError {
            index: index.0,
            source,
        };

        Ok(Section {
            index,
            address: section.address(),
            name: section.name().map_err(map_err)?,
            data: section.data().map_err(map_err)?,
            relocations: section
                .relocations()
                .map(|(offset, r)| {
                    Ok(Relocation {
                        symbol_index: match r.target() {
                            RelocationTarget::Symbol(index) => index.0,
                            _ => return Err(ParseError::UnsupportedRelocationTarget),
                        },
                        offset,
                    })
                })
                .collect::<Result<Vec<_>, _>>()?,
        })
    }
}

fn parse_license(data: &[u8]) -> Result<CString, ParseError> {
    if data.len() < 2 {
        return Err(ParseError::InvalidLicense {
            data: data.to_vec(),
        });
    }
    if data[data.len() - 1] != 0 {
        return Err(ParseError::MissingLicenseNullTerminator {
            data: data.to_vec(),
        });
    }

    Ok(CStr::from_bytes_with_nul(data)
        .map_err(|_| ParseError::InvalidLicense {
            data: data.to_vec(),
        })?
        .to_owned())
}

fn parse_version(data: &[u8], endianness: object::Endianness) -> Result<KernelVersion, ParseError> {
    let data = match data.len() {
        4 => data.try_into().unwrap(),
        _ => {
            return Err(ParseError::InvalidKernelVersion {
                data: data.to_vec(),
            })
        }
    };

    let v = match endianness {
        object::Endianness::Big => u32::from_be_bytes(data),
        object::Endianness::Little => u32::from_le_bytes(data),
    };

    Ok(match v {
        KERNEL_VERSION_ANY => KernelVersion::Any,
        v => KernelVersion::Version(v),
    })
}

#[derive(Copy, Clone, Debug, PartialEq)]
pub enum KernelVersion {
    Version(u32),
    Any,
}

impl From<KernelVersion> for u32 {
    fn from(version: KernelVersion) -> u32 {
        match version {
            KernelVersion::Any => KERNEL_VERSION_ANY,
            KernelVersion::Version(v) => v,
        }
    }
}

fn parse_map(section: &Section, name: &str) -> Result<Map, ParseError> {
    let (def, data) = if name == ".bss" || name.starts_with(".data") || name.starts_with(".rodata")
    {
        let def = bpf_map_def {
            map_type: BPF_MAP_TYPE_ARRAY as u32,
            key_size: mem::size_of::<u32>() as u32,
            value_size: section.data.len() as u32,
            max_entries: 1,
            map_flags: 0, /* FIXME: set rodata readonly */
            id: 0,
            pinning: 0,
        };
        (def, section.data.to_vec())
    } else {
        (parse_map_def(name, section.data)?, Vec::new())
    };

    Ok(Map {
        section_index: section.index.0,
        name: name.to_string(),
        def,
        data,
    })
}

fn parse_map_def(name: &str, data: &[u8]) -> Result<bpf_map_def, ParseError> {
    if mem::size_of::<bpf_map_def>() > data.len() {
        return Err(ParseError::InvalidMapDefinition {
            name: name.to_owned(),
        });
    }

    Ok(unsafe { ptr::read_unaligned(data.as_ptr() as *const bpf_map_def) })
}

fn copy_instructions(data: &[u8]) -> Result<Vec<bpf_insn>, ParseError> {
    if data.len() % mem::size_of::<bpf_insn>() > 0 {
        return Err(ParseError::InvalidProgramCode);
    }
    let num_instructions = data.len() / mem::size_of::<bpf_insn>();
    let instructions = (0..num_instructions)
        .map(|i| unsafe {
            ptr::read_unaligned(
                (data.as_ptr() as usize + i * mem::size_of::<bpf_insn>()) as *const bpf_insn,
            )
        })
        .collect::<Vec<_>>();

    Ok(instructions)
}

#[cfg(test)]
mod tests {
    use matches::assert_matches;
    use object::Endianness;
    use std::slice;

    use super::*;

    fn fake_section<'a>(name: &'a str, data: &'a [u8]) -> Section<'a> {
        Section {
            index: SectionIndex(0),
            address: 0,
            name,
            data,
            relocations: Vec::new(),
        }
    }

    fn fake_ins() -> bpf_insn {
        bpf_insn {
            code: 0,
            _bitfield_align_1: [],
            _bitfield_1: bpf_insn::new_bitfield_1(0, 0),
            off: 0,
            imm: 0,
        }
    }

    fn bytes_of<T>(val: &T) -> &[u8] {
        let size = mem::size_of::<T>();
        unsafe { slice::from_raw_parts(slice::from_ref(val).as_ptr().cast(), size) }
    }

    #[test]
    fn test_parse_generic_error() {
        assert!(matches!(
            Object::parse(&b"foo"[..]),
            Err(BpfError::ParseError(ParseError::ElfError(_)))
        ))
    }

    #[test]
    fn test_parse_license() {
        assert!(matches!(
            parse_license(b""),
            Err(ParseError::InvalidLicense { .. })
        ));

        assert!(matches!(
            parse_license(b"\0"),
            Err(ParseError::InvalidLicense { .. })
        ));

        assert!(matches!(
            parse_license(b"GPL"),
            Err(ParseError::MissingLicenseNullTerminator { .. })
        ));

        assert_eq!(parse_license(b"GPL\0").unwrap().to_str().unwrap(), "GPL");
    }

    #[test]
    fn test_parse_version() {
        assert!(matches!(
            parse_version(b"", Endianness::Little),
            Err(ParseError::InvalidKernelVersion { .. })
        ));

        assert!(matches!(
            parse_version(b"123", Endianness::Little),
            Err(ParseError::InvalidKernelVersion { .. })
        ));

        assert_eq!(
            parse_version(&0xFFFF_FFFEu32.to_le_bytes(), Endianness::Little)
                .expect("failed to parse magic version"),
            KernelVersion::Any
        );

        assert_eq!(
            parse_version(&0xFFFF_FFFEu32.to_be_bytes(), Endianness::Big)
                .expect("failed to parse magic version"),
            KernelVersion::Any
        );

        assert_eq!(
            parse_version(&1234u32.to_le_bytes(), Endianness::Little)
                .expect("failed to parse magic version"),
            KernelVersion::Version(1234)
        );
    }

    #[test]
    fn test_parse_map_def() {
        assert!(matches!(
            parse_map_def("foo", &[]),
            Err(ParseError::InvalidMapDefinition { .. })
        ));
        assert!(matches!(
            parse_map_def(
                "foo",
                bytes_of(&bpf_map_def {
                    map_type: 1,
                    key_size: 2,
                    value_size: 3,
                    max_entries: 4,
                    map_flags: 5,
                    id: 0,
                    pinning: 0
                })
            ),
            Ok(bpf_map_def {
                map_type: 1,
                key_size: 2,
                value_size: 3,
                max_entries: 4,
                map_flags: 5,
                id: 0,
                pinning: 0
            })
        ));
    }

    #[test]
    fn test_parse_map_error() {
        assert!(matches!(
            parse_map(&fake_section("maps/foo", &[]), "foo"),
            Err(ParseError::InvalidMapDefinition { .. })
        ))
    }

    #[test]
    fn test_parse_map() {
        assert!(matches!(
            parse_map(
                &fake_section(
                    "maps/foo",
                    bytes_of(&bpf_map_def {
                        map_type: 1,
                        key_size: 2,
                        value_size: 3,
                        max_entries: 4,
                        map_flags: 5,
                        id: 0,
                        pinning: 0
                    })
                ),
                "foo"
            ),
            Ok(Map {
                section_index: 0,
                name,
                def: bpf_map_def {
                    map_type: 1,
                    key_size: 2,
                    value_size: 3,
                    max_entries: 4,
                    map_flags: 5,
                    id: 0,
                    pinning: 0
                },
                data
            }) if name == "foo" && data.is_empty()
        ))
    }

    #[test]
    fn test_parse_map_data() {
        let map_data = b"map data";
        assert!(matches!(
            parse_map(
                &fake_section(
                    ".bss",
                    map_data,
                ),
                ".bss"
            ),
            Ok(Map {
                section_index: 0,
                name,
                def: bpf_map_def {
                    map_type: _map_type,
                    key_size: 4,
                    value_size,
                    max_entries: 1,
                    map_flags: 0,
                    id: 0,
                    pinning: 0
                },
                data
            }) if name == ".bss" && data == map_data && value_size == map_data.len() as u32
        ))
    }

    fn fake_obj() -> Object {
        Object::new(
            Endianness::Little,
            CString::new("GPL").unwrap(),
            KernelVersion::Any,
        )
    }

    #[test]
    fn test_parse_program_error() {
        let obj = fake_obj();

        assert_matches!(
            obj.parse_program(
                &fake_section("kprobe/foo", &42u32.to_ne_bytes(),),
                "kprobe",
                "foo"
            ),
            Err(ParseError::InvalidProgramCode)
        );
    }

    #[test]
    fn test_parse_program() {
        let obj = fake_obj();

        assert_matches!(
            obj.parse_program(&fake_section("kprobe/foo", bytes_of(&fake_ins())), "kprobe", "foo"),
            Ok(Program {
                license,
                kernel_version: KernelVersion::Any,
                kind: ProgramKind::KProbe,
                function: Function {
                    name,
                    address: 0,
                    section_index: SectionIndex(0),
                    section_offset: 0,
                    instructions
                }
            }) if license.to_string_lossy() == "GPL" && name == "foo" && instructions.len() == 1
        );
    }

    #[test]
    fn test_parse_section_map() {
        let mut obj = fake_obj();

        assert_matches!(
            obj.parse_section(fake_section(
                "maps/foo",
                bytes_of(&bpf_map_def {
                    map_type: 1,
                    key_size: 2,
                    value_size: 3,
                    max_entries: 4,
                    map_flags: 5,
                    id: 0,
                    pinning: 0
                })
            ),),
            Ok(())
        );
        assert!(obj.maps.get("foo").is_some());
    }

    #[test]
    fn test_parse_section_data() {
        let mut obj = fake_obj();
        assert_matches!(
            obj.parse_section(fake_section(".bss", b"map data"),),
            Ok(())
        );
        assert!(obj.maps.get(".bss").is_some());

        assert_matches!(
            obj.parse_section(fake_section(".rodata", b"map data"),),
            Ok(())
        );
        assert!(obj.maps.get(".rodata").is_some());

        assert_matches!(
            obj.parse_section(fake_section(".rodata.boo", b"map data"),),
            Ok(())
        );
        assert!(obj.maps.get(".rodata.boo").is_some());

        assert_matches!(
            obj.parse_section(fake_section(".data", b"map data"),),
            Ok(())
        );
        assert!(obj.maps.get(".data").is_some());

        assert_matches!(
            obj.parse_section(fake_section(".data.boo", b"map data"),),
            Ok(())
        );
        assert!(obj.maps.get(".data.boo").is_some());
    }

    #[test]
    fn test_parse_section_kprobe() {
        let mut obj = fake_obj();

        assert_matches!(
            obj.parse_section(fake_section("kprobe/foo", bytes_of(&fake_ins()))),
            Ok(())
        );
        assert_matches!(
            obj.programs.get("foo"),
            Some(Program {
                kind: ProgramKind::KProbe,
                ..
            })
        );
    }

    #[test]
    fn test_parse_section_uprobe() {
        let mut obj = fake_obj();

        assert_matches!(
            obj.parse_section(fake_section("uprobe/foo", bytes_of(&fake_ins()))),
            Ok(())
        );
        assert_matches!(
            obj.programs.get("foo"),
            Some(Program {
                kind: ProgramKind::UProbe,
                ..
            })
        );
    }

    #[test]
    fn test_parse_section_trace_point() {
        let mut obj = fake_obj();

        assert_matches!(
            obj.parse_section(fake_section("trace_point/foo", bytes_of(&fake_ins()))),
            Ok(())
        );
        assert_matches!(
            obj.programs.get("foo"),
            Some(Program {
                kind: ProgramKind::TracePoint,
                ..
            })
        );
    }

    #[test]
    fn test_parse_section_socket_filter() {
        let mut obj = fake_obj();

        assert_matches!(
            obj.parse_section(fake_section("socket_filter/foo", bytes_of(&fake_ins()))),
            Ok(())
        );
        assert_matches!(
            obj.programs.get("foo"),
            Some(Program {
                kind: ProgramKind::SocketFilter,
                ..
            })
        );
    }

    #[test]
    fn test_parse_section_xdp() {
        let mut obj = fake_obj();

        assert_matches!(
            obj.parse_section(fake_section("xdp/foo", bytes_of(&fake_ins()))),
            Ok(())
        );
        assert_matches!(
            obj.programs.get("foo"),
            Some(Program {
                kind: ProgramKind::Xdp,
                ..
            })
        );
    }
}
