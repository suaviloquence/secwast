mod instruction {
    crate::core::instructions! {
        pub enum Instruction<'a> {
            I32Load(MemArg<4>) : [0x28] : "i32.load",
            I64Load(MemArg<8>) : [0x29] : "i64.load",
            F32Load(MemArg<4>) : [0x2a] : "f32.load",
            F64Load(MemArg<8>) : [0x2b] : "f64.load",
            I32Load8s(MemArg<1>) : [0x2c] : "i32.load8_s",
            I32Load8u(MemArg<1>) : [0x2d] : "i32.load8_u",
            I32Load16s(MemArg<2>) : [0x2e] : "i32.load16_s",
            I32Load16u(MemArg<2>) : [0x2f] : "i32.load16_u",
            I64Load8s(MemArg<1>) : [0x30] : "i64.load8_s",
            I64Load8u(MemArg<1>) : [0x31] : "i64.load8_u",
            I64Load16s(MemArg<2>) : [0x32] : "i64.load16_s",
            I64Load16u(MemArg<2>) : [0x33] : "i64.load16_u",
            I64Load32s(MemArg<4>) : [0x34] : "i64.load32_s",
            I64Load32u(MemArg<4>) : [0x35] : "i64.load32_u",
            I32Store(MemArg<4>) : [0x36] : "i32.store",
            I64Store(MemArg<8>) : [0x37] : "i64.store",
            F32Store(MemArg<4>) : [0x38] : "f32.store",
            F64Store(MemArg<8>) : [0x39] : "f64.store",
            I32Store8(MemArg<1>) : [0x3a] : "i32.store8",
            I32Store16(MemArg<2>) : [0x3b] : "i32.store16",
            I64Store8(MemArg<1>) : [0x3c] : "i64.store8",
            I64Store16(MemArg<2>) : [0x3d] : "i64.store16",
            I64Store32(MemArg<4>) : [0x3e] : "i64.store32",
        }
    }
}

pub use instruction::Instruction as MemoryInstruction;

use crate::{
    core::{MemArg, ValType},
    encode::Encode,
    parser::{self, Parse, Parser},
};

/// A abstract syntactic representation of an information-flow label.
///
/// NB: the [`PartialEq`] and [`Eq`] implementations compare *syntactic* equality,
/// not necessarily *semantic* equivalence.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Label<'a> {
    /// Base case: a label \in \mathcal{L}
    Id(&'a str),
    // Join(Box<(Label<'a>, Label<'a>)>),
}

impl<'a> Parse<'a> for Label<'a> {
    fn parse(parser: Parser<'a>) -> parser::Result<Self> {
        Ok(Self::Id(parser.parse()?))
    }
}

mod annotation {
    crate::annotation!(label);
}

/// A `@label {label}` annotation on a type, memory instruction, or function.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LabelAnnotation<'a> {
    /// The label argument to the annotation.
    pub label: Label<'a>,
}

impl<'a> Parse<'a> for LabelAnnotation<'a> {
    fn parse(parser: Parser<'a>) -> parser::Result<Self> {
        parser.parse::<annotation::label>()?;
        Ok(Self {
            label: parser.parse()?,
        })
    }
}

/// A memory instruction which requires a syntactic `@label` annotation/
#[derive(Debug, Clone)]
pub struct LabeledInstruction<'a> {
    /// The label for the instruction
    pub label: LabelAnnotation<'a>,
    /// The memory instruction
    pub instruction: MemoryInstruction<'a>,
}

impl<'a> Parse<'a> for LabeledInstruction<'a> {
    fn parse(parser: Parser<'a>) -> parser::Result<Self> {
        Ok(Self {
            label: parser.parse()?,
            instruction: parser.parse()?,
        })
    }
}

/// All possible instructions in a SecWasm program.
#[derive(Debug, Clone)]
pub enum AllInstructions<'a> {
    /// A labeled memory instruction
    Labeled(LabeledInstruction<'a>),
    /// An instruction that does not need a label.
    Other(super::Instruction<'a>),
}

impl<'a> Parse<'a> for AllInstructions<'a> {
    fn parse(parser: Parser<'a>) -> parser::Result<Self> {
        let _guard = parser.register_annotation("label");
        if parser.peek::<annotation::label>()? {
            parser.parse().map(Self::Labeled)
        } else {
            match parser.parse() {
                Ok(other) => Ok(Self::Other(other)),
                Err(e) => Err(parser.error(format_args!("{e}.
                    note: if you tried to use a memory instruction, add a @label <label> annotation before it."))),
            }
        }
    }
}

impl Encode for AllInstructions<'_> {
    fn encode(&self, e: &mut Vec<u8>) {
        match self {
            Self::Labeled(l) => l.instruction.encode(e),
            Self::Other(o) => o.encode(e),
        }
    }
}

impl<'a> AllInstructions<'a> {
    pub(crate) fn needs_data_count(&self) -> bool {
        match self {
            Self::Labeled(_) => true,
            Self::Other(o) => o.needs_data_count(),
        }
    }

    pub(crate) fn memarg_mut(&mut self) -> Option<&mut MemArg<'a>> {
        match self {
            Self::Labeled(l) => l.instruction.memarg_mut(),
            Self::Other(o) => o.memarg_mut(),
        }
    }
}

/// A labeled value type \tau.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct LabeledValType<'a> {
    /// The label for the type
    pub label: LabelAnnotation<'a>,
    /// The base value type \in i32, i64, ...
    pub ty: ValType<'a>,
}

impl<'a> Parse<'a> for LabeledValType<'a> {
    fn parse(parser: Parser<'a>) -> parser::Result<Self> {
        let _guard = parser.register_annotation("label");
        Ok(Self {
            label: parser.parse()?,
            ty: parser.parse()?,
        })
    }
}

impl Encode for LabeledValType<'_> {
    fn encode(&self, e: &mut Vec<u8>) {
        self.ty.encode(e)
    }
}

impl From<LabeledValType<'_>> for wasm_encoder::ValType {
    fn from(value: LabeledValType<'_>) -> Self {
        value.ty.into()
    }
}
