use crate::annotation;
use crate::core::labels::AllInstructions;
use crate::core::*;
use crate::kw;
use crate::lexer::{Lexer, Token, TokenKind};
use crate::parser::{Parse, Parser, Result};
use crate::token::*;
use std::mem;

/// An expression, or a list of instructions, in the WebAssembly text format.
///
/// This expression type will parse s-expression-folded instructions into a flat
/// list of instructions for emission later on. The implicit `end` instruction
/// at the end of an expression is not included in the `instrs` field.
#[derive(Debug)]
#[allow(missing_docs)]
pub struct Expression<'a> {
    /// Instructions in this expression.
    pub instrs: Box<[AllInstructions<'a>]>,

    /// Branch hints, if any, found while parsing instructions.
    pub branch_hints: Box<[BranchHint]>,

    /// Optionally parsed spans of all instructions in `instrs`.
    ///
    /// This value is `None` as it's disabled by default. This can be enabled
    /// through the
    /// [`ParseBuffer::track_instr_spans`](crate::parser::ParseBuffer::track_instr_spans)
    /// function.
    ///
    /// This is not tracked by default due to the memory overhead and limited
    /// use of this field.
    pub instr_spans: Option<Box<[Span]>>,
}

/// A `@metadata.code.branch_hint` in the code, associated with a If or BrIf
/// This instruction is a placeholder and won't produce anything. Its purpose
/// is to store the offset of the following instruction and check that
/// it's followed by `br_if` or `if`.
#[derive(Debug)]
pub struct BranchHint {
    /// Index of instructions in `instrs` field of `Expression` that this hint
    /// applies to.
    pub instr_index: usize,
    /// The value of this branch hint
    pub value: u32,
}

impl<'a> Parse<'a> for Expression<'a> {
    fn parse(parser: Parser<'a>) -> Result<Self> {
        let mut exprs = ExpressionParser::new(parser);
        exprs.parse(parser)?;
        Ok(Expression {
            instrs: exprs.raw_instrs.into(),
            branch_hints: exprs.branch_hints.into(),
            instr_spans: exprs.spans.map(|s| s.into()),
        })
    }
}

impl<'a> Expression<'a> {
    /// Creates an expression from the single `instr` specified.
    pub fn one(instr: AllInstructions<'a>) -> Expression<'a> {
        Expression {
            instrs: [instr].into(),
            branch_hints: Box::new([]),
            instr_spans: None,
        }
    }

    /// Helper to create an expression from a single [`Instruction`]
    pub fn one_other(instr: Instruction<'a>) -> Self {
        Self::one(AllInstructions::Other(instr))
    }

    /// Parse an expression formed from a single folded instruction.
    ///
    /// Attempts to parse an expression formed from a single folded instruction.
    ///
    /// This method will mutate the state of `parser` after attempting to parse
    /// the expression. If an error happens then it is likely fatal and
    /// there is no guarantee of how many tokens have been consumed from
    /// `parser`.
    ///
    /// # Errors
    ///
    /// This function will return an error if the expression could not be
    /// parsed. Note that creating an [`crate::Error`] is not exactly a cheap
    /// operation, so [`crate::Error`] is typically fatal and propagated all the
    /// way back to the top parse call site.
    pub fn parse_folded_instruction(parser: Parser<'a>) -> Result<Self> {
        let mut exprs = ExpressionParser::new(parser);
        exprs.parse_folded_instruction(parser)?;
        Ok(Expression {
            instrs: exprs.raw_instrs.into(),
            branch_hints: exprs.branch_hints.into(),
            instr_spans: exprs.spans.map(|s| s.into()),
        })
    }
}

/// Helper struct used to parse an `Expression` with helper methods and such.
///
/// The primary purpose of this is to avoid defining expression parsing as a
/// call-thread-stack recursive function. Since we're parsing user input that
/// runs the risk of blowing the call stack, so we want to be sure to use a heap
/// stack structure wherever possible.
struct ExpressionParser<'a> {
    /// The flat list of instructions that we've parsed so far, and will
    /// eventually become the final `Expression`.
    ///
    /// Appended to with `push_instr` to ensure that this is the same length of
    /// `spans` if `spans` is used.
    raw_instrs: Vec<AllInstructions<'a>>,

    /// Descriptor of all our nested s-expr blocks. This only happens when
    /// instructions themselves are nested.
    stack: Vec<Level<'a>>,

    /// Related to the branch hints proposal.
    /// Will be used later to collect the offsets in the final binary.
    /// <(index of branch instructions, BranchHintAnnotation)>
    branch_hints: Vec<BranchHint>,

    /// Storage for all span information in `raw_instrs`. Optionally disabled to
    /// reduce memory consumption of parsing expressions.
    spans: Option<Vec<Span>>,
}

enum Paren {
    None,
    Left,
    Right(Span),
}

/// A "kind" of nested block that we can be parsing inside of.
enum Level<'a> {
    /// This is a normal `block` or `loop` or similar, where the instruction
    /// payload here is pushed when the block is exited.
    EndWith(AllInstructions<'a>, Option<Span>),

    /// This is a pretty special variant which means that we're parsing an `if`
    /// statement, and the state of the `if` parsing is tracked internally in
    /// the payload.
    If(If<'a>),

    /// This means we're either parsing inside of `(then ...)` or `(else ...)`
    /// which don't correspond to terminating instructions, we're just in a
    /// nested block.
    IfArm,

    /// This means we are finishing the parsing of a branch hint annotation.
    BranchHint,
}

/// Possible states of "what is currently being parsed?" in an `if` expression.
enum If<'a> {
    /// Only the `if` instruction has been parsed, next thing to parse is the
    /// clause, if any, of the `if` instruction.
    ///
    /// This parse ends when `(then ...)` is encountered.
    Clause(AllInstructions<'a>, Span),
    /// Currently parsing the `then` block, and afterwards a closing paren is
    /// required or an `(else ...)` expression.
    Then,
    /// Parsing the `else` expression, nothing can come after.
    Else,
}

impl<'a> ExpressionParser<'a> {
    fn new(parser: Parser<'a>) -> ExpressionParser<'a> {
        ExpressionParser {
            raw_instrs: Vec::new(),
            stack: Vec::new(),
            branch_hints: Vec::new(),
            spans: if parser.track_instr_spans() {
                Some(Vec::new())
            } else {
                None
            },
        }
    }

    fn parse(&mut self, parser: Parser<'a>) -> Result<()> {
        // Here we parse instructions in a loop, and we do not recursively
        // invoke this parse function to avoid blowing the stack on
        // deeply-recursive parses.
        //
        // Our loop generally only finishes once there's no more input left int
        // the `parser`. If there's some unclosed delimiters though (on our
        // `stack`), then we also keep parsing to generate error messages if
        // there's no input left.
        while !parser.is_empty() || !self.stack.is_empty() {
            // As a small ease-of-life adjustment here, if we're parsing inside
            // of an `if block then we require that all sub-components are
            // s-expressions surrounded by `(` and `)`, so verify that here.
            if let Some(Level::If(_)) = self.stack.last() {
                if !parser.is_empty() && !parser.peek::<LParen>()? {
                    return Err(parser.error("expected `(`"));
                }
            }

            match self.paren(parser)? {
                // No parenthesis seen? Then we just parse the next instruction
                // and move on.
                Paren::None => {
                    let span = parser.cur_span();
                    self.push_instr(parser.parse()?, span);
                }

                // If we see a left-parenthesis then things are a little
                // special. We handle block-like instructions specially
                // (`block`, `loop`, and `if`), and otherwise all other
                // instructions simply get appended once we reach the end of the
                // s-expression.
                //
                // In all cases here we push something onto the `stack` to get
                // popped when the `)` character is seen.
                Paren::Left => {
                    // First up is handling `if` parsing, which is funky in a
                    // whole bunch of ways. See the method internally for more
                    // information.
                    if self.handle_if_lparen(parser)? {
                        continue;
                    }

                    // Handle the case of a branch hint annotation
                    if parser.peek::<annotation::metadata_code_branch_hint>()? {
                        self.parse_branch_hint(parser)?;
                        self.stack.push(Level::BranchHint);
                        continue;
                    }

                    use AllInstructions::Other as O;

                    let span = parser.cur_span();
                    match parser.parse()? {
                        // If block/loop show up then we just need to be sure to
                        // push an `end` instruction whenever the `)` token is
                        // seen
                        i @ O(Instruction::Block(_))
                        | i @ O(Instruction::Loop(_))
                        | i @ O(Instruction::TryTable(_)) => {
                            self.push_instr(i, span);
                            self.stack.push(Level::EndWith(
                                AllInstructions::Other(Instruction::End(None)),
                                None,
                            ));
                        }

                        // Parsing an `if` instruction is super tricky, so we
                        // push an `If` scope and we let all our scope-based
                        // parsing handle the remaining items.
                        i @ O(Instruction::If(_)) => {
                            self.stack.push(Level::If(If::Clause(i, span)));
                        }

                        // Anything else means that we're parsing a nested form
                        // such as `(i32.add ...)` which means that the
                        // instruction we parsed will be coming at the end.
                        other => self.stack.push(Level::EndWith(other, Some(span))),
                    }
                }

                // If we registered a `)` token as being seen, then we're
                // guaranteed there's an item in the `stack` stack for us to
                // pop. We peel that off and take a look at what it says to do.
                Paren::Right(span) => match self.stack.pop().unwrap() {
                    Level::EndWith(i, s) => self.push_instr(i, s.unwrap_or(span)),
                    Level::IfArm => {}
                    Level::BranchHint => {}

                    // If an `if` statement hasn't parsed the clause or `then`
                    // block, then that's an error because there weren't enough
                    // items in the `if` statement. Otherwise we're just careful
                    // to terminate with an `end` instruction.
                    Level::If(If::Clause(..)) => {
                        return Err(parser.error("previous `if` had no `then`"));
                    }
                    Level::If(_) => {
                        self.push_instr(AllInstructions::Other(Instruction::End(None)), span);
                    }
                },
            }
        }
        Ok(())
    }

    fn parse_folded_instruction(&mut self, parser: Parser<'a>) -> Result<()> {
        let mut done = false;
        while !done {
            match self.paren(parser)? {
                Paren::Left => {
                    let span = parser.cur_span();
                    self.stack.push(Level::EndWith(parser.parse()?, Some(span)));
                }
                Paren::Right(span) => {
                    let (top_instr, span) = match self.stack.pop().unwrap() {
                        Level::EndWith(i, s) => (i, s.unwrap_or(span)),
                        _ => panic!("unknown level type"),
                    };
                    self.push_instr(top_instr, span);
                    if self.stack.is_empty() {
                        done = true;
                    }
                }
                Paren::None => {
                    return Err(parser.error("expected to continue a folded instruction"));
                }
            }
        }
        Ok(())
    }

    /// Parses either `(`, `)`, or nothing.
    fn paren(&self, parser: Parser<'a>) -> Result<Paren> {
        parser.step(|cursor| {
            Ok(match cursor.lparen()? {
                Some(rest) => (Paren::Left, rest),
                None if self.stack.is_empty() => (Paren::None, cursor),
                None => match cursor.rparen()? {
                    Some(rest) => (Paren::Right(cursor.cur_span()), rest),
                    None => (Paren::None, cursor),
                },
            })
        })
    }

    /// State transitions with parsing an `if` statement.
    ///
    /// The syntactical form of an `if` statement looks like:
    ///
    /// ```wat
    /// (if ($clause)... (then $then) (else $else))
    /// ```
    ///
    /// THis method is called after a `(` is parsed within the `(if ...` block.
    /// This determines what to do next.
    ///
    /// Returns `true` if the rest of the arm above should be skipped, or
    /// `false` if we should parse the next item as an instruction (because we
    /// didn't handle the lparen here).
    fn handle_if_lparen(&mut self, parser: Parser<'a>) -> Result<bool> {
        // Only execute the code below if there's an `If` listed last.
        let i = match self.stack.last_mut() {
            Some(Level::If(i)) => i,
            _ => return Ok(false),
        };

        match i {
            // If the clause is still being parsed then interpret this `(` as a
            // folded instruction unless it starts with `then`, in which case
            // this transitions to the `Then` state and a new level has been
            // reached.
            If::Clause(if_instr, if_instr_span) => {
                if !parser.peek::<kw::then>()? {
                    return Ok(false);
                }
                parser.parse::<kw::then>()?;
                let instr = mem::replace(if_instr, AllInstructions::Other(Instruction::End(None)));
                let span = *if_instr_span;
                *i = If::Then;
                self.push_instr(instr, span);
                self.stack.push(Level::IfArm);
                Ok(true)
            }

            // Previously we were parsing the `(then ...)` clause so this next
            // `(` must be followed by `else`.
            If::Then => {
                let span = parser.parse::<kw::r#else>()?.0;
                *i = If::Else;
                self.push_instr(AllInstructions::Other(Instruction::Else(None)), span);
                self.stack.push(Level::IfArm);
                Ok(true)
            }

            // If after a `(else ...` clause is parsed there's another `(` then
            // that's not syntactically allowed.
            If::Else => Err(parser.error("unexpected token: too many payloads inside of `(if)`")),
        }
    }

    fn parse_branch_hint(&mut self, parser: Parser<'a>) -> Result<()> {
        parser.parse::<annotation::metadata_code_branch_hint>()?;

        let hint = parser.parse::<String>()?;

        let value = match hint.as_bytes() {
            [0] => 0,
            [1] => 1,
            _ => return Err(parser.error("invalid value for branch hint")),
        };

        self.branch_hints.push(BranchHint {
            instr_index: self.raw_instrs.len(),
            value,
        });
        Ok(())
    }

    fn push_instr(&mut self, instr: AllInstructions<'a>, span: Span) {
        self.raw_instrs.push(instr);
        if let Some(spans) = &mut self.spans {
            spans.push(span);
        }
    }
}

// TODO: document this obscenity
macro_rules! instructions {
    (pub enum Instruction<'a> {
        $(
            $(#[$doc:meta])*
            $name:ident $(($($arg:tt)*))? : [$($binary:tt)*] : $instr:tt $( | $deprecated:tt )?,
        )*
    }) => (
        /// A listing of all WebAssembly instructions that can be in a module
        /// that this crate currently parses.
        #[derive(Debug, Clone)]
        #[allow(missing_docs)]
        pub enum Instruction<'a> {
            $(
                $(#[$doc])*
                $name $(( $crate::core::instructions!(@ty $($arg)*) ))?,
            )*
        }

        #[allow(non_snake_case)]
        impl<'a> $crate::parser::Parse<'a> for Instruction<'a> {
            fn parse(parser: $crate::parser::Parser<'a>) -> $crate::parser::Result<Self> {
                $(
                    fn $name<'a>(_parser: $crate::parser::Parser<'a>) -> $crate::parser::Result<Instruction<'a>> {
                        Ok(Instruction::$name $((
                            $crate::core::instructions!(@parse _parser $($arg)*)?
                        ))?)
                    }
                )*
                let parse_remainder = parser.step(|c| {
                    let (kw, rest) = match c.keyword() ?{
                        Some(pair) => pair,
                        None => return Err(c.error("expected an instruction")),
                    };
                    match kw {
                        $($instr $( | $deprecated )?=> Ok(($name as fn(_) -> _, rest)),)*
                        _ => return Err(c.error("unknown operator or unexpected token")),
                    }
                })?;
                parse_remainder(parser)
            }
        }

        impl $crate::encode::Encode for Instruction<'_> {
            #[allow(non_snake_case, unused_lifetimes)]
            fn encode(&self, v: &mut Vec<u8>) {
                match self {
                    $(
                        Instruction::$name $(($crate::core::instructions!(@first x $($arg)*)))? => {
                            fn encode<'a>($(arg: &$crate::core::instructions!(@ty $($arg)*),)? v: &mut Vec<u8>) {
                                $crate::core::instructions!(@encode v $($binary)*);
                                $(<$crate::core::instructions!(@ty $($arg)*) as $crate::encode::Encode>::encode(arg, v);)?
                            }
                            encode($( $crate::core::instructions!(@first x $($arg)*), )? v)
                        }
                    )*
                }
            }
        }

        impl<'a> Instruction<'a> {
            /// Returns the associated [`MemArg`] if one is available for this
            /// instruction.
            #[allow(unused_variables, non_snake_case)]
            pub fn memarg_mut(&mut self) -> Option<&mut $crate::core::MemArg<'a>> {
                match self {
                    $(
                        Instruction::$name $(($crate::core::instructions!(@memarg_binding a $($arg)*)))? => {
                            $crate::core::instructions!(@get_memarg a $($($arg)*)?)
                        }
                    )*
                }
            }
        }
    );

    (@ty MemArg<$amt:tt>) => ($crate::core::MemArg<'a>);
    (@ty $other:ty) => ($other);

    (@first $first:ident $($t:tt)*) => ($first);

    (@parse $parser:ident MemArg<$amt:tt>) => ($crate::core::MemArg::parse($parser, $amt));
    (@parse $parser:ident MemArg) => (compile_error!("must specify `MemArg` default"));
    (@parse $parser:ident LoadOrStoreLane<$amt:tt>) => (LoadOrStoreLane::parse($parser, $amt));
    (@parse $parser:ident LoadOrStoreLane) => (compile_error!("must specify `LoadOrStoreLane` default"));
    (@parse $parser:ident $other:ty) => ($parser.parse::<$other>());

    // simd opcodes prefixed with `0xfd` get a varuint32 encoding for their payload
    (@encode $dst:ident 0xfd, $simd:tt) => ({
        $dst.push(0xfd);
        <u32 as Encode>::encode(&$simd, $dst);
    });
    (@encode $dst:ident $($bytes:tt)*) => ($dst.extend_from_slice(&[$($bytes)*]););

    (@get_memarg $name:ident MemArg<$amt:tt>) => (Some($name));
    (@get_memarg $name:ident LoadOrStoreLane<$amt:tt>) => (Some(&mut $name.memarg));
    (@get_memarg $($other:tt)*) => (None);

    (@memarg_binding $name:ident MemArg<$amt:tt>) => ($name);
    (@memarg_binding $name:ident LoadOrStoreLane<$amt:tt>) => ($name);
    (@memarg_binding $name:ident $other:ty) => (_);
}

pub(super) use instructions;

instructions! {
    pub enum Instruction<'a> {
        Block(Box<BlockType<'a>>) : [0x02] : "block",
        If(Box<BlockType<'a>>) : [0x04] : "if",
        Else(Option<Id<'a>>) : [0x05] : "else",
        Loop(Box<BlockType<'a>>) : [0x03] : "loop",
        End(Option<Id<'a>>) : [0x0b] : "end",

        Unreachable : [0x00] : "unreachable",
        Nop : [0x01] : "nop",
        Br(Index<'a>) : [0x0c] : "br",
        BrIf(Index<'a>) : [0x0d] : "br_if",
        BrTable(BrTableIndices<'a>) : [0x0e] : "br_table",
        Return : [0x0f] : "return",
        Call(Index<'a>) : [0x10] : "call",
        CallIndirect(Box<CallIndirect<'a>>) : [0x11] : "call_indirect",

        // tail-call proposal
        ReturnCall(Index<'a>) : [0x12] : "return_call",
        ReturnCallIndirect(Box<CallIndirect<'a>>) : [0x13] : "return_call_indirect",

        // function-references proposal
        CallRef(Index<'a>) : [0x14] : "call_ref",
        ReturnCallRef(Index<'a>) : [0x15] : "return_call_ref",

        Drop : [0x1a] : "drop",
        Select(SelectTypes<'a>) : [] : "select",
        LocalGet(Index<'a>) : [0x20] : "local.get",
        LocalSet(Index<'a>) : [0x21] : "local.set",
        LocalTee(Index<'a>) : [0x22] : "local.tee",
        GlobalGet(Index<'a>) : [0x23] : "global.get",
        GlobalSet(Index<'a>) : [0x24] : "global.set",

        TableGet(TableArg<'a>) : [0x25] : "table.get",
        TableSet(TableArg<'a>) : [0x26] : "table.set",


        // Lots of bulk memory proposal here as well
        MemorySize(MemoryArg<'a>) : [0x3f] : "memory.size",
        MemoryGrow(MemoryArg<'a>) : [0x40] : "memory.grow",
        MemoryInit(MemoryInit<'a>) : [0xfc, 0x08] : "memory.init",
        MemoryCopy(MemoryCopy<'a>) : [0xfc, 0x0a] : "memory.copy",
        MemoryFill(MemoryArg<'a>) : [0xfc, 0x0b] : "memory.fill",
        MemoryDiscard(MemoryArg<'a>) : [0xfc, 0x12] : "memory.discard",
        DataDrop(Index<'a>) : [0xfc, 0x09] : "data.drop",
        ElemDrop(Index<'a>) : [0xfc, 0x0d] : "elem.drop",
        TableInit(TableInit<'a>) : [0xfc, 0x0c] : "table.init",
        TableCopy(TableCopy<'a>) : [0xfc, 0x0e] : "table.copy",
        TableFill(TableArg<'a>) : [0xfc, 0x11] : "table.fill",
        TableSize(TableArg<'a>) : [0xfc, 0x10] : "table.size",
        TableGrow(TableArg<'a>) : [0xfc, 0x0f] : "table.grow",

        RefNull(HeapType<'a>) : [0xd0] : "ref.null",
        RefIsNull : [0xd1] : "ref.is_null",
        RefFunc(Index<'a>) : [0xd2] : "ref.func",

        // function-references proposal
        RefAsNonNull : [0xd4] : "ref.as_non_null",
        BrOnNull(Index<'a>) : [0xd5] : "br_on_null",
        BrOnNonNull(Index<'a>) : [0xd6] : "br_on_non_null",
        // removed: gc proposal: eqref
        // removed: gc proposal: struct
        // removed: gc proposal: array
        // removed: gc proposal, i31
        // removed: gc proposal, concrete casting
        // removed: gc proposal extern/any coercion operations

        I32Const(i32) : [0x41] : "i32.const",
        I64Const(i64) : [0x42] : "i64.const",
        F32Const(F32) : [0x43] : "f32.const",
        F64Const(F64) : [0x44] : "f64.const",

        I32Clz : [0x67] : "i32.clz",
        I32Ctz : [0x68] : "i32.ctz",
        I32Popcnt : [0x69] : "i32.popcnt",
        I32Add : [0x6a] : "i32.add",
        I32Sub : [0x6b] : "i32.sub",
        I32Mul : [0x6c] : "i32.mul",
        I32DivS : [0x6d] : "i32.div_s",
        I32DivU : [0x6e] : "i32.div_u",
        I32RemS : [0x6f] : "i32.rem_s",
        I32RemU : [0x70] : "i32.rem_u",
        I32And : [0x71] : "i32.and",
        I32Or : [0x72] : "i32.or",
        I32Xor : [0x73] : "i32.xor",
        I32Shl : [0x74] : "i32.shl",
        I32ShrS : [0x75] : "i32.shr_s",
        I32ShrU : [0x76] : "i32.shr_u",
        I32Rotl : [0x77] : "i32.rotl",
        I32Rotr : [0x78] : "i32.rotr",

        I64Clz : [0x79] : "i64.clz",
        I64Ctz : [0x7a] : "i64.ctz",
        I64Popcnt : [0x7b] : "i64.popcnt",
        I64Add : [0x7c] : "i64.add",
        I64Sub : [0x7d] : "i64.sub",
        I64Mul : [0x7e] : "i64.mul",
        I64DivS : [0x7f] : "i64.div_s",
        I64DivU : [0x80] : "i64.div_u",
        I64RemS : [0x81] : "i64.rem_s",
        I64RemU : [0x82] : "i64.rem_u",
        I64And : [0x83] : "i64.and",
        I64Or : [0x84] : "i64.or",
        I64Xor : [0x85] : "i64.xor",
        I64Shl : [0x86] : "i64.shl",
        I64ShrS : [0x87] : "i64.shr_s",
        I64ShrU : [0x88] : "i64.shr_u",
        I64Rotl : [0x89] : "i64.rotl",
        I64Rotr : [0x8a] : "i64.rotr",

        F32Abs : [0x8b] : "f32.abs",
        F32Neg : [0x8c] : "f32.neg",
        F32Ceil : [0x8d] : "f32.ceil",
        F32Floor : [0x8e] : "f32.floor",
        F32Trunc : [0x8f] : "f32.trunc",
        F32Nearest : [0x90] : "f32.nearest",
        F32Sqrt : [0x91] : "f32.sqrt",
        F32Add : [0x92] : "f32.add",
        F32Sub : [0x93] : "f32.sub",
        F32Mul : [0x94] : "f32.mul",
        F32Div : [0x95] : "f32.div",
        F32Min : [0x96] : "f32.min",
        F32Max : [0x97] : "f32.max",
        F32Copysign : [0x98] : "f32.copysign",

        F64Abs : [0x99] : "f64.abs",
        F64Neg : [0x9a] : "f64.neg",
        F64Ceil : [0x9b] : "f64.ceil",
        F64Floor : [0x9c] : "f64.floor",
        F64Trunc : [0x9d] : "f64.trunc",
        F64Nearest : [0x9e] : "f64.nearest",
        F64Sqrt : [0x9f] : "f64.sqrt",
        F64Add : [0xa0] : "f64.add",
        F64Sub : [0xa1] : "f64.sub",
        F64Mul : [0xa2] : "f64.mul",
        F64Div : [0xa3] : "f64.div",
        F64Min : [0xa4] : "f64.min",
        F64Max : [0xa5] : "f64.max",
        F64Copysign : [0xa6] : "f64.copysign",

        I32Eqz : [0x45] : "i32.eqz",
        I32Eq : [0x46] : "i32.eq",
        I32Ne : [0x47] : "i32.ne",
        I32LtS : [0x48] : "i32.lt_s",
        I32LtU : [0x49] : "i32.lt_u",
        I32GtS : [0x4a] : "i32.gt_s",
        I32GtU : [0x4b] : "i32.gt_u",
        I32LeS : [0x4c] : "i32.le_s",
        I32LeU : [0x4d] : "i32.le_u",
        I32GeS : [0x4e] : "i32.ge_s",
        I32GeU : [0x4f] : "i32.ge_u",

        I64Eqz : [0x50] : "i64.eqz",
        I64Eq : [0x51] : "i64.eq",
        I64Ne : [0x52] : "i64.ne",
        I64LtS : [0x53] : "i64.lt_s",
        I64LtU : [0x54] : "i64.lt_u",
        I64GtS : [0x55] : "i64.gt_s",
        I64GtU : [0x56] : "i64.gt_u",
        I64LeS : [0x57] : "i64.le_s",
        I64LeU : [0x58] : "i64.le_u",
        I64GeS : [0x59] : "i64.ge_s",
        I64GeU : [0x5a] : "i64.ge_u",

        F32Eq : [0x5b] : "f32.eq",
        F32Ne : [0x5c] : "f32.ne",
        F32Lt : [0x5d] : "f32.lt",
        F32Gt : [0x5e] : "f32.gt",
        F32Le : [0x5f] : "f32.le",
        F32Ge : [0x60] : "f32.ge",

        F64Eq : [0x61] : "f64.eq",
        F64Ne : [0x62] : "f64.ne",
        F64Lt : [0x63] : "f64.lt",
        F64Gt : [0x64] : "f64.gt",
        F64Le : [0x65] : "f64.le",
        F64Ge : [0x66] : "f64.ge",

        I32WrapI64 : [0xa7] : "i32.wrap_i64",
        I32TruncF32S : [0xa8] : "i32.trunc_f32_s",
        I32TruncF32U : [0xa9] : "i32.trunc_f32_u",
        I32TruncF64S : [0xaa] : "i32.trunc_f64_s",
        I32TruncF64U : [0xab] : "i32.trunc_f64_u",
        I64ExtendI32S : [0xac] : "i64.extend_i32_s",
        I64ExtendI32U : [0xad] : "i64.extend_i32_u",
        I64TruncF32S : [0xae] : "i64.trunc_f32_s",
        I64TruncF32U : [0xaf] : "i64.trunc_f32_u",
        I64TruncF64S : [0xb0] : "i64.trunc_f64_s",
        I64TruncF64U : [0xb1] : "i64.trunc_f64_u",
        F32ConvertI32S : [0xb2] : "f32.convert_i32_s",
        F32ConvertI32U : [0xb3] : "f32.convert_i32_u",
        F32ConvertI64S : [0xb4] : "f32.convert_i64_s",
        F32ConvertI64U : [0xb5] : "f32.convert_i64_u",
        F32DemoteF64 : [0xb6] : "f32.demote_f64",
        F64ConvertI32S : [0xb7] : "f64.convert_i32_s",
        F64ConvertI32U : [0xb8] : "f64.convert_i32_u",
        F64ConvertI64S : [0xb9] : "f64.convert_i64_s",
        F64ConvertI64U : [0xba] : "f64.convert_i64_u",
        F64PromoteF32 : [0xbb] : "f64.promote_f32",
        I32ReinterpretF32 : [0xbc] : "i32.reinterpret_f32",
        I64ReinterpretF64 : [0xbd] : "i64.reinterpret_f64",
        F32ReinterpretI32 : [0xbe] : "f32.reinterpret_i32",
        F64ReinterpretI64 : [0xbf] : "f64.reinterpret_i64",

        // non-trapping float to int
        I32TruncSatF32S : [0xfc, 0x00] : "i32.trunc_sat_f32_s",
        I32TruncSatF32U : [0xfc, 0x01] : "i32.trunc_sat_f32_u",
        I32TruncSatF64S : [0xfc, 0x02] : "i32.trunc_sat_f64_s",
        I32TruncSatF64U : [0xfc, 0x03] : "i32.trunc_sat_f64_u",
        I64TruncSatF32S : [0xfc, 0x04] : "i64.trunc_sat_f32_s",
        I64TruncSatF32U : [0xfc, 0x05] : "i64.trunc_sat_f32_u",
        I64TruncSatF64S : [0xfc, 0x06] : "i64.trunc_sat_f64_s",
        I64TruncSatF64U : [0xfc, 0x07] : "i64.trunc_sat_f64_u",

        // sign extension proposal
        I32Extend8S : [0xc0] : "i32.extend8_s",
        I32Extend16S : [0xc1] : "i32.extend16_s",
        I64Extend8S : [0xc2] : "i64.extend8_s",
        I64Extend16S : [0xc3] : "i64.extend16_s",
        I64Extend32S : [0xc4] : "i64.extend32_s",

        // removed: atomics proposal
        // removed: proposal: shared-everything-threads
        // removed: proposal: simd
        // Exception handling proposal
        ThrowRef : [0x0a] : "throw_ref",
        TryTable(TryTable<'a>) : [0x1f] : "try_table",
        Throw(Index<'a>) : [0x08] : "throw",

        // Deprecated exception handling opcodes
        Try(Box<BlockType<'a>>) : [0x06] : "try",
        Catch(Index<'a>) : [0x07] : "catch",
        Rethrow(Index<'a>) : [0x09] : "rethrow",
        Delegate(Index<'a>) : [0x18] : "delegate",
        CatchAll : [0x19] : "catch_all",

        // removed: Relaxed SIMD proposal
        // removed: Stack switching proposal
        // removed: Wide arithmetic proposal
    }
}

// As shown in #1095 the size of this variant is somewhat performance-sensitive
// since big `*.wat` files will have a lot of these. This is a small ratchet to
// make sure that this enum doesn't become larger than it already is, although
// ideally it also wouldn't be as large as it is now.
#[test]
fn assert_instruction_not_too_large() {
    let size = std::mem::size_of::<Instruction<'_>>();
    let pointer = std::mem::size_of::<u64>();
    assert!(size <= pointer * 11);
}

impl<'a> Instruction<'a> {
    pub(crate) fn needs_data_count(&self) -> bool {
        match self {
            Instruction::MemoryInit(_) | Instruction::DataDrop(_) => true,
            _ => false,
        }
    }
}

/// Extra information associated with block-related instructions.
///
/// This is used to label blocks and also annotate what types are expected for
/// the block.
#[derive(Debug, Clone)]
#[allow(missing_docs)]
pub struct BlockType<'a> {
    pub label: Option<Id<'a>>,
    pub label_name: Option<NameAnnotation<'a>>,
    pub ty: TypeUse<'a, FunctionType<'a>>,
}

impl<'a> Parse<'a> for BlockType<'a> {
    fn parse(parser: Parser<'a>) -> Result<Self> {
        Ok(BlockType {
            label: parser.parse()?,
            label_name: parser.parse()?,
            ty: parser
                .parse::<TypeUse<'a, FunctionTypeNoNames<'a>>>()?
                .into(),
        })
    }
}

/// Extra information associated with the cont.bind instruction
#[derive(Debug, Clone)]
#[allow(missing_docs)]
pub struct ContBind<'a> {
    pub argument_index: Index<'a>,
    pub result_index: Index<'a>,
}

impl<'a> Parse<'a> for ContBind<'a> {
    fn parse(parser: Parser<'a>) -> Result<Self> {
        Ok(ContBind {
            argument_index: parser.parse()?,
            result_index: parser.parse()?,
        })
    }
}

/// Extra information associated with the resume instruction
#[derive(Debug, Clone)]
#[allow(missing_docs)]
pub struct Resume<'a> {
    pub type_index: Index<'a>,
    pub table: ResumeTable<'a>,
}

impl<'a> Parse<'a> for Resume<'a> {
    fn parse(parser: Parser<'a>) -> Result<Self> {
        Ok(Resume {
            type_index: parser.parse()?,
            table: parser.parse()?,
        })
    }
}

/// Extra information associated with the resume_throw instruction
#[derive(Debug, Clone)]
#[allow(missing_docs)]
pub struct ResumeThrow<'a> {
    pub type_index: Index<'a>,
    pub tag_index: Index<'a>,
    pub table: ResumeTable<'a>,
}

impl<'a> Parse<'a> for ResumeThrow<'a> {
    fn parse(parser: Parser<'a>) -> Result<Self> {
        Ok(ResumeThrow {
            type_index: parser.parse()?,
            tag_index: parser.parse()?,
            table: parser.parse()?,
        })
    }
}

/// Extra information associated with the switch instruction
#[derive(Debug, Clone)]
#[allow(missing_docs)]
pub struct Switch<'a> {
    pub type_index: Index<'a>,
    pub tag_index: Index<'a>,
}

impl<'a> Parse<'a> for Switch<'a> {
    fn parse(parser: Parser<'a>) -> Result<Self> {
        Ok(Switch {
            type_index: parser.parse()?,
            tag_index: parser.parse()?,
        })
    }
}

/// A representation of resume tables
#[derive(Debug, Clone)]
#[allow(missing_docs)]
pub struct ResumeTable<'a> {
    pub handlers: Vec<Handle<'a>>,
}

/// A representation of resume table entries
#[derive(Debug, Clone)]
#[allow(missing_docs)]
pub enum Handle<'a> {
    OnLabel { tag: Index<'a>, label: Index<'a> },
    OnSwitch { tag: Index<'a> },
}

impl<'a> Parse<'a> for ResumeTable<'a> {
    fn parse(parser: Parser<'a>) -> Result<Self> {
        let mut handlers = Vec::new();
        while parser.peek::<LParen>()? && parser.peek2::<kw::on>()? {
            handlers.push(parser.parens(|p| {
                p.parse::<kw::on>()?;
                let tag: Index<'a> = p.parse()?;
                if p.peek::<kw::switch>()? {
                    p.parse::<kw::switch>()?;
                    Ok(Handle::OnSwitch { tag })
                } else {
                    Ok(Handle::OnLabel {
                        tag,
                        label: p.parse()?,
                    })
                }
            })?);
        }
        Ok(ResumeTable { handlers })
    }
}

#[derive(Debug, Clone)]
#[allow(missing_docs)]
pub struct TryTable<'a> {
    pub block: Box<BlockType<'a>>,
    pub catches: Vec<TryTableCatch<'a>>,
}

impl<'a> Parse<'a> for TryTable<'a> {
    fn parse(parser: Parser<'a>) -> Result<Self> {
        let block = parser.parse()?;

        let mut catches = Vec::new();
        while parser.peek::<LParen>()?
            && (parser.peek2::<kw::catch>()?
                || parser.peek2::<kw::catch_ref>()?
                || parser.peek2::<kw::catch_all>()?
                || parser.peek2::<kw::catch_all_ref>()?)
        {
            catches.push(parser.parens(|p| {
                let kind = if parser.peek::<kw::catch_ref>()? {
                    p.parse::<kw::catch_ref>()?;
                    TryTableCatchKind::CatchRef(p.parse()?)
                } else if parser.peek::<kw::catch>()? {
                    p.parse::<kw::catch>()?;
                    TryTableCatchKind::Catch(p.parse()?)
                } else if parser.peek::<kw::catch_all>()? {
                    p.parse::<kw::catch_all>()?;
                    TryTableCatchKind::CatchAll
                } else {
                    p.parse::<kw::catch_all_ref>()?;
                    TryTableCatchKind::CatchAllRef
                };

                Ok(TryTableCatch {
                    kind,
                    label: p.parse()?,
                })
            })?);
        }

        Ok(TryTable { block, catches })
    }
}

#[derive(Debug, Clone)]
#[allow(missing_docs)]
pub enum TryTableCatchKind<'a> {
    // Catch a tagged exception, do not capture an exnref.
    Catch(Index<'a>),
    // Catch a tagged exception, and capture the exnref.
    CatchRef(Index<'a>),
    // Catch any exception, do not capture an exnref.
    CatchAll,
    // Catch any exception, and capture the exnref.
    CatchAllRef,
}

impl<'a> TryTableCatchKind<'a> {
    #[allow(missing_docs)]
    pub fn tag_index_mut(&mut self) -> Option<&mut Index<'a>> {
        match self {
            TryTableCatchKind::Catch(tag) | TryTableCatchKind::CatchRef(tag) => Some(tag),
            TryTableCatchKind::CatchAll | TryTableCatchKind::CatchAllRef => None,
        }
    }
}

#[derive(Debug, Clone)]
#[allow(missing_docs)]
pub struct TryTableCatch<'a> {
    pub kind: TryTableCatchKind<'a>,
    pub label: Index<'a>,
}

/// Extra information associated with the `br_table` instruction.
#[allow(missing_docs)]
#[derive(Debug, Clone)]
pub struct BrTableIndices<'a> {
    pub labels: Vec<Index<'a>>,
    pub default: Index<'a>,
}

impl<'a> Parse<'a> for BrTableIndices<'a> {
    fn parse(parser: Parser<'a>) -> Result<Self> {
        let mut labels = vec![parser.parse()?];
        while parser.peek::<Index>()? {
            labels.push(parser.parse()?);
        }
        let default = labels.pop().unwrap();
        Ok(BrTableIndices { labels, default })
    }
}

/// Payload for lane-related instructions. Unsigned with no + prefix.
#[derive(Debug, Clone)]
pub struct LaneArg {
    /// The lane argument.
    pub lane: u8,
}

impl<'a> Parse<'a> for LaneArg {
    fn parse(parser: Parser<'a>) -> Result<Self> {
        let lane = parser.step(|c| {
            if let Some((i, rest)) = c.integer()? {
                if i.sign() == None {
                    let (src, radix) = i.val();
                    let val = u8::from_str_radix(src, radix)
                        .map_err(|_| c.error("malformed lane index"))?;
                    Ok((val, rest))
                } else {
                    Err(c.error("unexpected token"))
                }
            } else {
                Err(c.error("expected a lane index"))
            }
        })?;
        Ok(LaneArg { lane })
    }
}

/// Payload for memory-related instructions indicating offset/alignment of
/// memory accesses.
#[derive(Debug, Clone)]
pub struct MemArg<'a> {
    /// The alignment of this access.
    ///
    /// This is not stored as a log, this is the actual alignment (e.g. 1, 2, 4,
    /// 8, etc).
    pub align: u64,
    /// The offset, in bytes of this access.
    pub offset: u64,
    /// The memory index we're accessing
    pub memory: Index<'a>,
}

impl<'a> MemArg<'a> {
    pub(super) fn parse(parser: Parser<'a>, default_align: u64) -> Result<Self> {
        fn parse_field(name: &str, parser: Parser<'_>) -> Result<Option<u64>> {
            parser.step(|c| {
                let (kw, rest) = match c.keyword()? {
                    Some(p) => p,
                    None => return Ok((None, c)),
                };
                if !kw.starts_with(name) {
                    return Ok((None, c));
                }
                let kw = &kw[name.len()..];
                if !kw.starts_with('=') {
                    return Ok((None, c));
                }
                let num = &kw[1..];
                let lexer = Lexer::new(num);
                let mut pos = 0;
                if let Ok(Some(
                    token @ Token {
                        kind: TokenKind::Integer(integer_kind),
                        ..
                    },
                )) = lexer.parse(&mut pos)
                {
                    let int = token.integer(lexer.input(), integer_kind);
                    let (s, base) = int.val();
                    let value = u64::from_str_radix(s, base);
                    return match value {
                        Ok(n) => Ok((Some(n), rest)),
                        Err(_) => Err(c.error("u64 constant out of range")),
                    };
                }
                Err(c.error("expected u64 integer constant"))
            })
        }

        let memory = parser
            .parse::<Option<_>>()?
            .unwrap_or_else(|| Index::Num(0, parser.prev_span()));
        let offset = parse_field("offset", parser)?.unwrap_or(0);
        let align = match parse_field("align", parser)? {
            Some(n) if !n.is_power_of_two() => {
                return Err(parser.error("alignment must be a power of two"));
            }
            n => n.unwrap_or(default_align),
        };

        Ok(MemArg {
            offset,
            align,
            memory,
        })
    }
}

/// Extra data associated with the `call_indirect` instruction.
#[derive(Debug, Clone)]
pub struct CallIndirect<'a> {
    /// The table that this call is going to be indexing.
    pub table: Index<'a>,
    /// Type type signature that this `call_indirect` instruction is using.
    pub ty: TypeUse<'a, FunctionType<'a>>,
}

impl<'a> Parse<'a> for CallIndirect<'a> {
    fn parse(parser: Parser<'a>) -> Result<Self> {
        let prev_span = parser.prev_span();
        let table: Option<_> = parser.parse()?;
        let ty = parser.parse::<TypeUse<'a, FunctionTypeNoNames<'a>>>()?;
        Ok(CallIndirect {
            table: table.unwrap_or(Index::Num(0, prev_span)),
            ty: ty.into(),
        })
    }
}

/// Extra data associated with the `table.init` instruction
#[derive(Debug, Clone)]
pub struct TableInit<'a> {
    /// The index of the table we're copying into.
    pub table: Index<'a>,
    /// The index of the element segment we're copying into a table.
    pub elem: Index<'a>,
}

impl<'a> Parse<'a> for TableInit<'a> {
    fn parse(parser: Parser<'a>) -> Result<Self> {
        let prev_span = parser.prev_span();
        let (elem, table) = if parser.peek2::<Index>()? {
            let table = parser.parse()?;
            (parser.parse()?, table)
        } else {
            (parser.parse()?, Index::Num(0, prev_span))
        };
        Ok(TableInit { table, elem })
    }
}

/// Extra data associated with the `table.copy` instruction.
#[derive(Debug, Clone)]
pub struct TableCopy<'a> {
    /// The index of the destination table to copy into.
    pub dst: Index<'a>,
    /// The index of the source table to copy from.
    pub src: Index<'a>,
}

impl<'a> Parse<'a> for TableCopy<'a> {
    fn parse(parser: Parser<'a>) -> Result<Self> {
        let (dst, src) = match parser.parse::<Option<_>>()? {
            Some(dst) => (dst, parser.parse()?),
            None => (
                Index::Num(0, parser.prev_span()),
                Index::Num(0, parser.prev_span()),
            ),
        };
        Ok(TableCopy { dst, src })
    }
}

/// Extra data associated with unary table instructions.
#[derive(Debug, Clone)]
pub struct TableArg<'a> {
    /// The index of the table argument.
    pub dst: Index<'a>,
}

// `TableArg` could be an unwrapped as an `Index` if not for this custom parse
// behavior: if we cannot parse a table index, we default to table `0`.
impl<'a> Parse<'a> for TableArg<'a> {
    fn parse(parser: Parser<'a>) -> Result<Self> {
        let dst = if let Some(dst) = parser.parse()? {
            dst
        } else {
            Index::Num(0, parser.prev_span())
        };
        Ok(TableArg { dst })
    }
}

/// Extra data associated with unary memory instructions.
#[derive(Debug, Clone)]
pub struct MemoryArg<'a> {
    /// The index of the memory space.
    pub mem: Index<'a>,
}

impl<'a> Parse<'a> for MemoryArg<'a> {
    fn parse(parser: Parser<'a>) -> Result<Self> {
        let mem = if let Some(mem) = parser.parse()? {
            mem
        } else {
            Index::Num(0, parser.prev_span())
        };
        Ok(MemoryArg { mem })
    }
}

/// Extra data associated with the `memory.init` instruction
#[derive(Debug, Clone)]
pub struct MemoryInit<'a> {
    /// The index of the data segment we're copying into memory.
    pub data: Index<'a>,
    /// The index of the memory we're copying into,
    pub mem: Index<'a>,
}

impl<'a> Parse<'a> for MemoryInit<'a> {
    fn parse(parser: Parser<'a>) -> Result<Self> {
        let prev_span = parser.prev_span();
        let (data, mem) = if parser.peek2::<Index>()? {
            let memory = parser.parse()?;
            (parser.parse()?, memory)
        } else {
            (parser.parse()?, Index::Num(0, prev_span))
        };
        Ok(MemoryInit { data, mem })
    }
}

/// Extra data associated with the `memory.copy` instruction
#[derive(Debug, Clone)]
pub struct MemoryCopy<'a> {
    /// The index of the memory we're copying from.
    pub src: Index<'a>,
    /// The index of the memory we're copying to.
    pub dst: Index<'a>,
}

impl<'a> Parse<'a> for MemoryCopy<'a> {
    fn parse(parser: Parser<'a>) -> Result<Self> {
        let (src, dst) = match parser.parse()? {
            Some(dst) => (parser.parse()?, dst),
            None => (
                Index::Num(0, parser.prev_span()),
                Index::Num(0, parser.prev_span()),
            ),
        };
        Ok(MemoryCopy { src, dst })
    }
}

/// Extra data associated with the `struct.get/set` instructions
#[derive(Debug, Clone)]
pub struct StructAccess<'a> {
    /// The index of the struct type we're accessing.
    pub r#struct: Index<'a>,
    /// The index of the field of the struct we're accessing
    pub field: Index<'a>,
}

impl<'a> Parse<'a> for StructAccess<'a> {
    fn parse(parser: Parser<'a>) -> Result<Self> {
        Ok(StructAccess {
            r#struct: parser.parse()?,
            field: parser.parse()?,
        })
    }
}

/// Extra data associated with the `array.fill` instruction
#[derive(Debug, Clone)]
pub struct ArrayFill<'a> {
    /// The index of the array type we're filling.
    pub array: Index<'a>,
}

impl<'a> Parse<'a> for ArrayFill<'a> {
    fn parse(parser: Parser<'a>) -> Result<Self> {
        Ok(ArrayFill {
            array: parser.parse()?,
        })
    }
}

/// Extra data associated with the `array.copy` instruction
#[derive(Debug, Clone)]
pub struct ArrayCopy<'a> {
    /// The index of the array type we're copying to.
    pub dest_array: Index<'a>,
    /// The index of the array type we're copying from.
    pub src_array: Index<'a>,
}

impl<'a> Parse<'a> for ArrayCopy<'a> {
    fn parse(parser: Parser<'a>) -> Result<Self> {
        Ok(ArrayCopy {
            dest_array: parser.parse()?,
            src_array: parser.parse()?,
        })
    }
}

/// Extra data associated with the `array.init_[data/elem]` instruction
#[derive(Debug, Clone)]
pub struct ArrayInit<'a> {
    /// The index of the array type we're initializing.
    pub array: Index<'a>,
    /// The index of the data or elem segment we're reading from.
    pub segment: Index<'a>,
}

impl<'a> Parse<'a> for ArrayInit<'a> {
    fn parse(parser: Parser<'a>) -> Result<Self> {
        Ok(ArrayInit {
            array: parser.parse()?,
            segment: parser.parse()?,
        })
    }
}

/// Extra data associated with the `array.new_fixed` instruction
#[derive(Debug, Clone)]
pub struct ArrayNewFixed<'a> {
    /// The index of the array type we're accessing.
    pub array: Index<'a>,
    /// The amount of values to initialize the array with.
    pub length: u32,
}

impl<'a> Parse<'a> for ArrayNewFixed<'a> {
    fn parse(parser: Parser<'a>) -> Result<Self> {
        Ok(ArrayNewFixed {
            array: parser.parse()?,
            length: parser.parse()?,
        })
    }
}

/// Extra data associated with the `array.new_data` instruction
#[derive(Debug, Clone)]
pub struct ArrayNewData<'a> {
    /// The index of the array type we're accessing.
    pub array: Index<'a>,
    /// The data segment to initialize from.
    pub data_idx: Index<'a>,
}

impl<'a> Parse<'a> for ArrayNewData<'a> {
    fn parse(parser: Parser<'a>) -> Result<Self> {
        Ok(ArrayNewData {
            array: parser.parse()?,
            data_idx: parser.parse()?,
        })
    }
}

/// Extra data associated with the `array.new_elem` instruction
#[derive(Debug, Clone)]
pub struct ArrayNewElem<'a> {
    /// The index of the array type we're accessing.
    pub array: Index<'a>,
    /// The elem segment to initialize from.
    pub elem_idx: Index<'a>,
}

impl<'a> Parse<'a> for ArrayNewElem<'a> {
    fn parse(parser: Parser<'a>) -> Result<Self> {
        Ok(ArrayNewElem {
            array: parser.parse()?,
            elem_idx: parser.parse()?,
        })
    }
}

/// Extra data associated with the `ref.cast` instruction
#[derive(Debug, Clone)]
pub struct RefCast<'a> {
    /// The type to cast to.
    pub r#type: RefType<'a>,
}

impl<'a> Parse<'a> for RefCast<'a> {
    fn parse(parser: Parser<'a>) -> Result<Self> {
        Ok(RefCast {
            r#type: parser.parse()?,
        })
    }
}

/// Extra data associated with the `ref.test` instruction
#[derive(Debug, Clone)]
pub struct RefTest<'a> {
    /// The type to test for.
    pub r#type: RefType<'a>,
}

impl<'a> Parse<'a> for RefTest<'a> {
    fn parse(parser: Parser<'a>) -> Result<Self> {
        Ok(RefTest {
            r#type: parser.parse()?,
        })
    }
}

/// Extra data associated with the `br_on_cast` instruction
#[derive(Debug, Clone)]
pub struct BrOnCast<'a> {
    /// The label to branch to.
    pub label: Index<'a>,
    /// The type we're casting from.
    pub from_type: RefType<'a>,
    /// The type we're casting to.
    pub to_type: RefType<'a>,
}

impl<'a> Parse<'a> for BrOnCast<'a> {
    fn parse(parser: Parser<'a>) -> Result<Self> {
        Ok(BrOnCast {
            label: parser.parse()?,
            from_type: parser.parse()?,
            to_type: parser.parse()?,
        })
    }
}

/// Extra data associated with the `br_on_cast_fail` instruction
#[derive(Debug, Clone)]
pub struct BrOnCastFail<'a> {
    /// The label to branch to.
    pub label: Index<'a>,
    /// The type we're casting from.
    pub from_type: RefType<'a>,
    /// The type we're casting to.
    pub to_type: RefType<'a>,
}

impl<'a> Parse<'a> for BrOnCastFail<'a> {
    fn parse(parser: Parser<'a>) -> Result<Self> {
        Ok(BrOnCastFail {
            label: parser.parse()?,
            from_type: parser.parse()?,
            to_type: parser.parse()?,
        })
    }
}

/// The memory ordering for atomic instructions.
///
/// For an in-depth explanation of memory orderings, see the C++ documentation
/// for [`memory_order`] or the Rust documentation for [`atomic::Ordering`].
///
/// [`memory_order`]: https://en.cppreference.com/w/cpp/atomic/memory_order
/// [`atomic::Ordering`]: https://doc.rust-lang.org/std/sync/atomic/enum.Ordering.html
#[derive(Clone, Debug)]
pub enum Ordering {
    /// Like `AcqRel` but all threads see all sequentially consistent operations
    /// in the same order.
    AcqRel,
    /// For a load, it acquires; this orders all operations before the last
    /// "releasing" store. For a store, it releases; this orders all operations
    /// before it at the next "acquiring" load.
    SeqCst,
}

impl<'a> Parse<'a> for Ordering {
    fn parse(parser: Parser<'a>) -> Result<Self> {
        if parser.peek::<kw::seq_cst>()? {
            parser.parse::<kw::seq_cst>()?;
            Ok(Ordering::SeqCst)
        } else if parser.peek::<kw::acq_rel>()? {
            parser.parse::<kw::acq_rel>()?;
            Ok(Ordering::AcqRel)
        } else {
            Err(parser.error("expected a memory ordering: `seq_cst` or `acq_rel`"))
        }
    }
}

/// Add a memory [`Ordering`] to the argument `T` of some instruction.
///
/// This is helpful for many kinds of `*.atomic.*` instructions introduced by
/// the shared-everything-threads proposal. Many of these instructions "build
/// on" existing instructions by simply adding a memory order to them.
#[derive(Clone, Debug)]
pub struct Ordered<T> {
    /// The memory ordering for this atomic instruction.
    pub ordering: Ordering,
    /// The original argument type.
    pub inner: T,
}

impl<'a, T> Parse<'a> for Ordered<T>
where
    T: Parse<'a>,
{
    fn parse(parser: Parser<'a>) -> Result<Self> {
        let ordering = parser.parse()?;
        let inner = parser.parse()?;
        Ok(Ordered { ordering, inner })
    }
}

/// Different ways to specify a `v128.const` instruction
#[derive(Clone, Debug)]
#[allow(missing_docs)]
pub enum V128Const {
    I8x16([i8; 16]),
    I16x8([i16; 8]),
    I32x4([i32; 4]),
    I64x2([i64; 2]),
    F32x4([F32; 4]),
    F64x2([F64; 2]),
}

impl V128Const {
    /// Returns the raw little-ended byte sequence used to represent this
    /// `v128` constant`
    ///
    /// This is typically suitable for encoding as the payload of the
    /// `v128.const` instruction.
    #[rustfmt::skip]
    pub fn to_le_bytes(&self) -> [u8; 16] {
        match self {
            V128Const::I8x16(arr) => [
                arr[0] as u8,
                arr[1] as u8,
                arr[2] as u8,
                arr[3] as u8,
                arr[4] as u8,
                arr[5] as u8,
                arr[6] as u8,
                arr[7] as u8,
                arr[8] as u8,
                arr[9] as u8,
                arr[10] as u8,
                arr[11] as u8,
                arr[12] as u8,
                arr[13] as u8,
                arr[14] as u8,
                arr[15] as u8,
            ],
            V128Const::I16x8(arr) => {
                let a1 = arr[0].to_le_bytes();
                let a2 = arr[1].to_le_bytes();
                let a3 = arr[2].to_le_bytes();
                let a4 = arr[3].to_le_bytes();
                let a5 = arr[4].to_le_bytes();
                let a6 = arr[5].to_le_bytes();
                let a7 = arr[6].to_le_bytes();
                let a8 = arr[7].to_le_bytes();
                [
                    a1[0], a1[1],
                    a2[0], a2[1],
                    a3[0], a3[1],
                    a4[0], a4[1],
                    a5[0], a5[1],
                    a6[0], a6[1],
                    a7[0], a7[1],
                    a8[0], a8[1],
                ]
            }
            V128Const::I32x4(arr) => {
                let a1 = arr[0].to_le_bytes();
                let a2 = arr[1].to_le_bytes();
                let a3 = arr[2].to_le_bytes();
                let a4 = arr[3].to_le_bytes();
                [
                    a1[0], a1[1], a1[2], a1[3],
                    a2[0], a2[1], a2[2], a2[3],
                    a3[0], a3[1], a3[2], a3[3],
                    a4[0], a4[1], a4[2], a4[3],
                ]
            }
            V128Const::I64x2(arr) => {
                let a1 = arr[0].to_le_bytes();
                let a2 = arr[1].to_le_bytes();
                [
                    a1[0], a1[1], a1[2], a1[3], a1[4], a1[5], a1[6], a1[7],
                    a2[0], a2[1], a2[2], a2[3], a2[4], a2[5], a2[6], a2[7],
                ]
            }
            V128Const::F32x4(arr) => {
                let a1 = arr[0].bits.to_le_bytes();
                let a2 = arr[1].bits.to_le_bytes();
                let a3 = arr[2].bits.to_le_bytes();
                let a4 = arr[3].bits.to_le_bytes();
                [
                    a1[0], a1[1], a1[2], a1[3],
                    a2[0], a2[1], a2[2], a2[3],
                    a3[0], a3[1], a3[2], a3[3],
                    a4[0], a4[1], a4[2], a4[3],
                ]
            }
            V128Const::F64x2(arr) => {
                let a1 = arr[0].bits.to_le_bytes();
                let a2 = arr[1].bits.to_le_bytes();
                [
                    a1[0], a1[1], a1[2], a1[3], a1[4], a1[5], a1[6], a1[7],
                    a2[0], a2[1], a2[2], a2[3], a2[4], a2[5], a2[6], a2[7],
                ]
            }
        }
    }
}

impl<'a> Parse<'a> for V128Const {
    fn parse(parser: Parser<'a>) -> Result<Self> {
        let mut l = parser.lookahead1();
        if l.peek::<kw::i8x16>()? {
            parser.parse::<kw::i8x16>()?;
            Ok(V128Const::I8x16([
                parser.parse()?,
                parser.parse()?,
                parser.parse()?,
                parser.parse()?,
                parser.parse()?,
                parser.parse()?,
                parser.parse()?,
                parser.parse()?,
                parser.parse()?,
                parser.parse()?,
                parser.parse()?,
                parser.parse()?,
                parser.parse()?,
                parser.parse()?,
                parser.parse()?,
                parser.parse()?,
            ]))
        } else if l.peek::<kw::i16x8>()? {
            parser.parse::<kw::i16x8>()?;
            Ok(V128Const::I16x8([
                parser.parse()?,
                parser.parse()?,
                parser.parse()?,
                parser.parse()?,
                parser.parse()?,
                parser.parse()?,
                parser.parse()?,
                parser.parse()?,
            ]))
        } else if l.peek::<kw::i32x4>()? {
            parser.parse::<kw::i32x4>()?;
            Ok(V128Const::I32x4([
                parser.parse()?,
                parser.parse()?,
                parser.parse()?,
                parser.parse()?,
            ]))
        } else if l.peek::<kw::i64x2>()? {
            parser.parse::<kw::i64x2>()?;
            Ok(V128Const::I64x2([parser.parse()?, parser.parse()?]))
        } else if l.peek::<kw::f32x4>()? {
            parser.parse::<kw::f32x4>()?;
            Ok(V128Const::F32x4([
                parser.parse()?,
                parser.parse()?,
                parser.parse()?,
                parser.parse()?,
            ]))
        } else if l.peek::<kw::f64x2>()? {
            parser.parse::<kw::f64x2>()?;
            Ok(V128Const::F64x2([parser.parse()?, parser.parse()?]))
        } else {
            Err(l.error())
        }
    }
}

/// Lanes being shuffled in the `i8x16.shuffle` instruction
#[derive(Debug, Clone)]
pub struct I8x16Shuffle {
    #[allow(missing_docs)]
    pub lanes: [u8; 16],
}

impl<'a> Parse<'a> for I8x16Shuffle {
    fn parse(parser: Parser<'a>) -> Result<Self> {
        Ok(I8x16Shuffle {
            lanes: [
                parser.parse()?,
                parser.parse()?,
                parser.parse()?,
                parser.parse()?,
                parser.parse()?,
                parser.parse()?,
                parser.parse()?,
                parser.parse()?,
                parser.parse()?,
                parser.parse()?,
                parser.parse()?,
                parser.parse()?,
                parser.parse()?,
                parser.parse()?,
                parser.parse()?,
                parser.parse()?,
            ],
        })
    }
}

/// Payload of the `select` instructions
#[derive(Debug, Clone)]
pub struct SelectTypes<'a> {
    #[allow(missing_docs)]
    pub tys: Option<Vec<LabeledValType<'a>>>,
}

impl<'a> Parse<'a> for SelectTypes<'a> {
    fn parse(parser: Parser<'a>) -> Result<Self> {
        let mut found = false;
        let mut list = Vec::new();
        while parser.peek2::<kw::result>()? {
            found = true;
            parser.parens(|p| {
                p.parse::<kw::result>()?;
                while !p.is_empty() {
                    list.push(p.parse()?);
                }
                Ok(())
            })?;
        }
        Ok(SelectTypes {
            tys: if found { Some(list) } else { None },
        })
    }
}
