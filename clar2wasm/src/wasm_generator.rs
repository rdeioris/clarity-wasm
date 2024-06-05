use std::borrow::BorrowMut;
use std::collections::HashMap;

use clarity::vm::analysis::ContractAnalysis;
use clarity::vm::diagnostic::DiagnosableError;
use clarity::vm::types::signatures::{StringUTF8Length, BUFF_1};
use clarity::vm::types::{
    CharType, FixedFunction, FunctionType, PrincipalData, SequenceData, SequenceSubtype,
    StringSubtype, TypeSignature,
};
use clarity::vm::variables::NativeVariables;
use clarity::vm::{functions, variables, ClarityName, SymbolicExpression, SymbolicExpressionType};
use walrus::ir::{
    BinaryOp, IfElse, InstrSeqId, InstrSeqType, LoadKind, MemArg, StoreKind, UnaryOp,
};
use walrus::{
    ActiveData, DataKind, FunctionBuilder, FunctionId, GlobalId, InstrSeqBuilder, LocalId,
    MemoryId, Module, ValType,
};

use crate::wasm_utils::{
    get_type_in_memory_size, get_type_size, is_in_memory_type, ordered_tuple_signature,
    owned_ordered_tuple_signature,
};
use crate::words;

// First free position after data directly defined in standard.wat
pub const END_OF_STANDARD_DATA: u32 = 1352;

/// WasmGenerator is a Clarity AST visitor that generates a WebAssembly module
/// as it traverses the AST.
pub struct WasmGenerator {
    /// The contract analysis, which contains the expressions and type
    /// information for the contract.
    pub(crate) contract_analysis: ContractAnalysis,
    /// The WebAssembly module that is being generated.
    pub(crate) module: Module,
    /// Offset of the end of the literal memory.
    pub(crate) literal_memory_end: u32,
    /// Global ID of the stack pointer.
    pub(crate) stack_pointer: GlobalId,
    /// Map strings saved in the literal memory to their offset.
    pub(crate) literal_memory_offset: HashMap<LiteralMemoryEntry, u32>,
    /// Map constants to an offset in the literal memory.
    pub(crate) constants: HashMap<String, u32>,
    /// The current function body block, used for early exit
    early_return_block_id: Option<InstrSeqId>,
    /// The return type of the current function.
    pub(crate) return_type: Option<TypeSignature>,
    /// The types of defined data-vars
    pub(crate) datavars_types: HashMap<ClarityName, TypeSignature>,
    /// The types of (key, value) in defined maps
    pub(crate) maps_types: HashMap<ClarityName, (TypeSignature, TypeSignature)>,

    /// The locals for the current function.
    pub(crate) bindings: HashMap<String, Vec<LocalId>>,
    /// Size of the current function's stack frame.
    frame_size: i32,
}

#[derive(Hash, Eq, PartialEq)]
pub enum LiteralMemoryEntry {
    Ascii(String),
    Utf8(String),
}

#[derive(Debug)]
pub enum GeneratorError {
    NotImplemented,
    InternalError(String),
    TypeError(String),
}

pub enum FunctionKind {
    Public,
    Private,
    ReadOnly,
}

impl DiagnosableError for GeneratorError {
    fn message(&self) -> String {
        match self {
            GeneratorError::NotImplemented => "Not implemented".to_string(),
            GeneratorError::InternalError(msg) => format!("Internal error: {}", msg),
            GeneratorError::TypeError(msg) => format!("Type error: {}", msg),
        }
    }

    fn suggestion(&self) -> Option<String> {
        None
    }
}

pub trait ArgumentsExt {
    fn get_expr(&self, n: usize) -> Result<&SymbolicExpression, GeneratorError>;
    fn get_name(&self, n: usize) -> Result<&ClarityName, GeneratorError>;
    fn get_list(&self, n: usize) -> Result<&[SymbolicExpression], GeneratorError>;
}

impl ArgumentsExt for &[SymbolicExpression] {
    fn get_expr(&self, n: usize) -> Result<&SymbolicExpression, GeneratorError> {
        self.get(n).ok_or_else(|| {
            GeneratorError::InternalError(format!(
                "{self:?} does not have an argument of index {n}"
            ))
        })
    }

    fn get_name(&self, n: usize) -> Result<&ClarityName, GeneratorError> {
        self.get_expr(n)?.match_atom().ok_or_else(|| {
            GeneratorError::InternalError(format!(
                "{self:?} does not have a name at argument index {n}"
            ))
        })
    }

    fn get_list(&self, n: usize) -> Result<&[SymbolicExpression], GeneratorError> {
        self.get_expr(n)?.match_list().ok_or_else(|| {
            GeneratorError::InternalError(format!(
                "{self:?} does not have a list at argument index {n}"
            ))
        })
    }
}

/// Push a placeholder value for Wasm type `ty` onto the data stack.
/// `unreachable!` is used for Wasm types that should never be used.
#[allow(clippy::unreachable)]
pub(crate) fn add_placeholder_for_type(builder: &mut InstrSeqBuilder, ty: ValType) {
    match ty {
        ValType::I32 => builder.i32_const(0),
        ValType::I64 => builder.i64_const(0),
        ValType::F32 | ValType::F64 | ValType::V128 | ValType::Externref | ValType::Funcref => {
            unreachable!("Use of Wasm type {}", ty);
        }
    };
}

/// Push a placeholder value for Clarity type `ty` onto the data stack.
pub(crate) fn add_placeholder_for_clarity_type(builder: &mut InstrSeqBuilder, ty: &TypeSignature) {
    let wasm_types = clar2wasm_ty(ty);
    for wasm_type in wasm_types.iter() {
        add_placeholder_for_type(builder, *wasm_type);
    }
}

/// Convert a Clarity type signature to a wasm type signature.
pub(crate) fn clar2wasm_ty(ty: &TypeSignature) -> Vec<ValType> {
    match ty {
        TypeSignature::NoType => vec![ValType::I32], // TODO: can this just be empty?
        TypeSignature::IntType => vec![ValType::I64, ValType::I64],
        TypeSignature::UIntType => vec![ValType::I64, ValType::I64],
        TypeSignature::ResponseType(inner_types) => {
            let mut types = vec![ValType::I32];
            types.extend(clar2wasm_ty(&inner_types.0));
            types.extend(clar2wasm_ty(&inner_types.1));
            types
        }
        TypeSignature::SequenceType(_) | TypeSignature::ListUnionType(_) => vec![
            ValType::I32, // offset
            ValType::I32, // length
        ],
        TypeSignature::BoolType => vec![ValType::I32],
        TypeSignature::PrincipalType
        | TypeSignature::CallableType(_)
        | TypeSignature::TraitReferenceType(_) => vec![
            ValType::I32, // offset
            ValType::I32, // length
        ],
        TypeSignature::OptionalType(inner_ty) => {
            let mut types = vec![ValType::I32];
            types.extend(clar2wasm_ty(inner_ty));
            types
        }
        TypeSignature::TupleType(inner_types) => {
            let mut types = vec![];
            for &inner_type in ordered_tuple_signature(inner_types).values() {
                types.extend(clar2wasm_ty(inner_type));
            }
            types
        }
    }
}

pub enum SequenceElementType {
    /// A byte, from a string-ascii or buffer.
    Byte,
    /// A 32-bit unicode scalar value, from a string-utf8.
    UnicodeScalar,
    /// Any other type.
    Other(TypeSignature),
}

/// Drop a value of type `ty` from the data stack.
pub(crate) fn drop_value(builder: &mut InstrSeqBuilder, ty: &TypeSignature) {
    let wasm_types = clar2wasm_ty(ty);
    (0..wasm_types.len()).for_each(|_| {
        builder.drop();
    });
}

pub fn type_from_sequence_element(se: &SequenceElementType) -> TypeSignature {
    match se {
        SequenceElementType::Other(o) => o.clone(),
        SequenceElementType::Byte => BUFF_1.clone(),
        SequenceElementType::UnicodeScalar => {
            TypeSignature::SequenceType(SequenceSubtype::StringType(StringSubtype::UTF8(
                #[allow(clippy::unwrap_used)]
                StringUTF8Length::try_from(1u32).unwrap(),
            )))
        }
    }
}

impl WasmGenerator {
    pub fn new(contract_analysis: ContractAnalysis) -> Result<WasmGenerator, GeneratorError> {
        let standard_lib_wasm: &[u8] = include_bytes!("standard/standard.wasm");

        let module = Module::from_buffer(standard_lib_wasm).map_err(|_err| {
            GeneratorError::InternalError("failed to load standard library".to_owned())
        })?;
        // Get the stack-pointer global ID
        let stack_pointer_name = "stack-pointer";
        let global_id = module
            .globals
            .iter()
            .find(|global| {
                global
                    .name
                    .as_ref()
                    .map_or(false, |name| name == stack_pointer_name)
            })
            .map(|global| global.id())
            .ok_or_else(|| {
                GeneratorError::InternalError(
                    "Expected to find a global named $stack-pointer".to_owned(),
                )
            })?;

        Ok(WasmGenerator {
            contract_analysis,
            module,
            literal_memory_end: END_OF_STANDARD_DATA,
            stack_pointer: global_id,
            literal_memory_offset: HashMap::new(),
            constants: HashMap::new(),
            bindings: HashMap::new(),
            early_return_block_id: None,
            return_type: None,
            frame_size: 0,
            datavars_types: HashMap::new(),
            maps_types: HashMap::new(),
        })
    }

    pub fn set_memory_pages(&mut self) -> Result<(), GeneratorError> {
        let memory = self
            .module
            .memories
            .iter_mut()
            .next()
            .ok_or_else(|| GeneratorError::InternalError("No Memory found".to_owned()))?;

        let total_memory_bytes = self.literal_memory_end + (self.frame_size as u32);
        let pages_required = total_memory_bytes / (64 * 1024);
        let remainder = total_memory_bytes % (64 * 1024);

        memory.initial = pages_required + (remainder > 0) as u32;

        Ok(())
    }

    pub fn generate(mut self) -> Result<Module, GeneratorError> {
        let expressions = std::mem::take(&mut self.contract_analysis.expressions);

        // Get the type of the last top-level expression with a return value
        // or default to `None`.
        let return_ty = expressions
            .iter()
            .rev()
            .find_map(|expr| self.get_expr_type(expr))
            .map_or_else(Vec::new, clar2wasm_ty);

        let mut current_function = FunctionBuilder::new(&mut self.module.types, &[], &return_ty);

        if !expressions.is_empty() {
            self.traverse_statement_list(&mut current_function.func_body(), &expressions)?;
        }

        self.contract_analysis.expressions = expressions;

        let top_level = current_function.finish(vec![], &mut self.module.funcs);
        self.module.exports.add(".top-level", top_level);

        self.set_memory_pages()?;

        // Update the initial value of the stack-pointer to point beyond the
        // literal memory.
        self.module.globals.get_mut(self.stack_pointer).kind = walrus::GlobalKind::Local(
            walrus::InitExpr::Value(walrus::ir::Value::I32(self.literal_memory_end as i32)),
        );

        Ok(self.module)
    }

    pub fn get_memory(&self) -> Result<MemoryId, GeneratorError> {
        Ok(self
            .module
            .memories
            .iter()
            .next()
            .ok_or(GeneratorError::InternalError("No memory found".to_owned()))?
            .id())
    }

    pub fn traverse_expr(
        &mut self,
        builder: &mut InstrSeqBuilder,
        expr: &SymbolicExpression,
    ) -> Result<(), GeneratorError> {
        match &expr.expr {
            SymbolicExpressionType::Atom(name) => self.visit_atom(builder, expr, name),
            SymbolicExpressionType::List(exprs) => self.traverse_list(builder, expr, exprs),
            SymbolicExpressionType::LiteralValue(value) => {
                self.visit_literal_value(builder, expr, value)
            }
            _ => Ok(()),
        }
    }

    fn traverse_list(
        &mut self,
        builder: &mut InstrSeqBuilder,
        expr: &SymbolicExpression,
        list: &[SymbolicExpression],
    ) -> Result<(), GeneratorError> {
        match list.split_first() {
            Some((
                SymbolicExpression {
                    expr: SymbolicExpressionType::Atom(function_name),
                    ..
                },
                args,
            )) => {
                // Extract the types from the args and return
                let get_types = || {
                    let arg_types: Result<Vec<TypeSignature>, GeneratorError> = args
                        .iter()
                        .map(|e| {
                            self.get_expr_type(e).cloned().ok_or_else(|| {
                                GeneratorError::TypeError("expected valid argument type".to_owned())
                            })
                        })
                        .collect();
                    let return_type = self
                        .get_expr_type(expr)
                        .ok_or_else(|| {
                            GeneratorError::TypeError("Simple words must be typed".to_owned())
                        })
                        .cloned();
                    Ok((arg_types?, return_type?))
                };

                // Complex words handle their own argument traversal, and have priority
                // since we need to have a slight overlap for the words `and` and `or`
                // which exist in both complex and simple forms
                if let Some(word) = words::lookup_complex(function_name) {
                    word.traverse(self, builder, expr, args)?;
                } else if let Some(simpleword) = words::lookup_simple(function_name) {
                    let (arg_types, return_type) = get_types()?;

                    // traverse arguments
                    for arg in args {
                        self.traverse_expr(builder, arg)?;
                    }

                    simpleword.visit(self, builder, &arg_types, &return_type)?;
                } else if let Some(variadic) = words::lookup_variadic_simple(function_name) {
                    let (arg_types, return_type) = get_types()?;

                    let mut args_enumerated = args.iter().enumerate();

                    let first_arg = args_enumerated
                        .next()
                        .ok_or_else(|| {
                            GeneratorError::InternalError(
                                "Variadic called without arguments".to_owned(),
                            )
                        })?
                        .1;

                    self.traverse_expr(builder, first_arg)?;

                    if arg_types.len() == 1 {
                        variadic.visit(self, builder, &arg_types[..1], &return_type)?;
                    } else {
                        for (i, expr) in args_enumerated {
                            self.traverse_expr(builder, expr)?;
                            variadic.visit(self, builder, &arg_types[i - 1..=i], &return_type)?;
                        }
                    }

                    // first argument is traversed outside loop
                } else {
                    self.traverse_call_user_defined(builder, expr, function_name, args)?;
                }
            }
            _ => return Err(GeneratorError::InternalError("Invalid list".into())),
        }
        Ok(())
    }

    pub fn traverse_define_function(
        &mut self,
        builder: &mut InstrSeqBuilder,
        name: &ClarityName,
        body: &SymbolicExpression,
        kind: FunctionKind,
    ) -> Result<FunctionId, GeneratorError> {
        let opt_function_type = match kind {
            FunctionKind::ReadOnly => {
                builder.i32_const(0);
                self.contract_analysis
                    .get_read_only_function_type(name.as_str())
            }
            FunctionKind::Public => {
                builder.i32_const(1);
                self.contract_analysis
                    .get_public_function_type(name.as_str())
            }
            FunctionKind::Private => {
                builder.i32_const(2);
                self.contract_analysis.get_private_function(name.as_str())
            }
        };
        let function_type = if let Some(FunctionType::Fixed(fixed)) = opt_function_type {
            fixed.clone()
        } else {
            return Err(GeneratorError::TypeError(match opt_function_type {
                Some(_) => "expected fixed function type".to_string(),
                None => format!("unable to find function type for {}", name.as_str()),
            }));
        };

        self.return_type = Some(function_type.returns.clone());

        // Call the host interface to save this function
        // Arguments are kind (already pushed) and name (offset, length)
        let (id_offset, id_length) = self.add_string_literal(name)?;
        builder
            .i32_const(id_offset as i32)
            .i32_const(id_length as i32);

        // Call the host interface function, `define_function`
        builder.call(self.func_by_name("stdlib.define_function"));

        let mut bindings = HashMap::new();

        // Setup the parameters
        let mut param_locals = Vec::new();
        let mut params_types = Vec::new();
        for param in function_type.args.iter() {
            let param_types = clar2wasm_ty(&param.signature);
            let mut plocals = Vec::with_capacity(param_types.len());
            for ty in param_types {
                let local = self.module.locals.add(ty);
                param_locals.push(local);
                plocals.push(local);
                params_types.push(ty);
            }
            bindings.insert(param.name.to_string(), plocals.clone());
        }

        let results_types = clar2wasm_ty(&function_type.returns);
        let mut func_builder = FunctionBuilder::new(
            &mut self.module.types,
            params_types.as_slice(),
            results_types.as_slice(),
        );
        func_builder.name(name.as_str().to_string());
        let mut func_body = func_builder.func_body();

        // Function prelude
        // Save the frame pointer in a local variable.
        let frame_pointer = self.module.locals.add(ValType::I32);
        func_body
            .global_get(self.stack_pointer)
            .local_set(frame_pointer);

        // Setup the locals map for this function, saving the top-level map to
        // restore after.
        let top_level_locals = std::mem::replace(&mut self.bindings, bindings);

        let mut block = func_body.dangling_instr_seq(InstrSeqType::new(
            &mut self.module.types,
            &[],
            results_types.as_slice(),
        ));
        let block_id = block.id();

        self.early_return_block_id = Some(block_id);

        // Traverse the body of the function
        self.set_expr_type(body, function_type.returns.clone())?;
        self.traverse_expr(&mut block, body)?;

        // Insert the function body block into the function
        func_body.instr(walrus::ir::Block { seq: block_id });

        // Function postlude
        // Restore the initial stack pointer.
        func_body
            .local_get(frame_pointer)
            .global_set(self.stack_pointer);

        // Restore the top-level locals map.
        self.bindings = top_level_locals;

        // Reset the return type and early block to None
        self.return_type = None;
        self.early_return_block_id = None;

        Ok(func_builder.finish(param_locals, &mut self.module.funcs))
    }

    pub fn return_early(&self, builder: &mut InstrSeqBuilder) -> Result<(), GeneratorError> {
        if let Some(block_id) = self.early_return_block_id {
            builder.instr(walrus::ir::Br { block: block_id });
        } else {
            // This must be from a top-leve statement, so it should cause a runtime error
            builder
                .i32_const(7)
                .call(self.func_by_name("stdlib.runtime-error"));
            builder.unreachable();
        }

        Ok(())
    }

    /// Gets the result type of the given `SymbolicExpression`.
    pub fn get_expr_type(&self, expr: &SymbolicExpression) -> Option<&TypeSignature> {
        self.contract_analysis
            .type_map
            .as_ref()
            .and_then(|ty| ty.get_type_expected(expr))
    }

    /// Sets the result type of the given `SymbolicExpression`. This is
    /// necessary to overcome some weaknesses in the type-checker and
    /// hopefully can be removed in the future.
    pub fn set_expr_type(
        &mut self,
        expr: &SymbolicExpression,
        ty: TypeSignature,
    ) -> Result<(), GeneratorError> {
        // Safely ignore the error because we know this type has already been set.
        let _ = self
            .contract_analysis
            .type_map
            .as_mut()
            .ok_or_else(|| {
                GeneratorError::InternalError(
                    "type-checker must be called before Wasm generation".to_owned(),
                )
            })?
            .set_type(expr, ty);
        Ok(())
    }

    /// Adds a new string literal into the memory, and returns the offset and length.
    pub(crate) fn add_clarity_string_literal(
        &mut self,
        s: &CharType,
    ) -> Result<(u32, u32), GeneratorError> {
        // If this string has already been saved in the literal memory,
        // just return the offset and length.
        let (data, entry) = match s {
            CharType::ASCII(s) => {
                let entry = LiteralMemoryEntry::Ascii(s.to_string());
                if let Some(offset) = self.literal_memory_offset.get(&entry) {
                    return Ok((*offset, s.data.len() as u32));
                }
                (s.data.clone(), entry)
            }
            CharType::UTF8(u) => {
                let data_str = String::from_utf8(u.data.iter().flatten().cloned().collect())
                    .map_err(|_e| {
                        GeneratorError::InternalError("Invalid UTF-8 sequence".to_owned())
                    })?;
                let entry = LiteralMemoryEntry::Utf8(data_str.clone());
                if let Some(offset) = self.literal_memory_offset.get(&entry) {
                    return Ok((*offset, u.data.len() as u32 * 4));
                }
                // Convert the string into 4-byte big-endian unicode scalar values.
                let data = data_str
                    .chars()
                    .flat_map(|c| (c as u32).to_be_bytes())
                    .collect();
                (data, entry)
            }
        };
        let memory = self.get_memory()?;
        let offset = self.literal_memory_end;
        let len = data.len() as u32;
        self.module.data.add(
            DataKind::Active(ActiveData {
                memory,
                location: walrus::ActiveDataLocation::Absolute(offset),
            }),
            data,
        );
        self.literal_memory_end += len;

        // Save the offset in the literal memory for this string
        self.literal_memory_offset.insert(entry, offset);

        Ok((offset, len))
    }

    /// Adds a new string literal into the memory for an identifier
    pub(crate) fn add_string_literal(&mut self, name: &str) -> Result<(u32, u32), GeneratorError> {
        // If this identifier has already been saved in the literal memory,
        // just return the offset and length.
        let entry = LiteralMemoryEntry::Ascii(name.to_string());
        if let Some(offset) = self.literal_memory_offset.get(&entry) {
            return Ok((*offset, name.len() as u32));
        }

        let memory = self.get_memory()?;
        let offset = self.literal_memory_end;
        let len = name.len() as u32;
        self.module.data.add(
            DataKind::Active(ActiveData {
                memory,
                location: walrus::ActiveDataLocation::Absolute(offset),
            }),
            name.as_bytes().to_vec(),
        );
        self.literal_memory_end += name.len() as u32;

        // Save the offset in the literal memory for this identifier
        self.literal_memory_offset.insert(entry, offset);

        Ok((offset, len))
    }

    /// Adds a new literal into the memory, and returns the offset and length.
    pub(crate) fn add_literal(
        &mut self,
        value: &clarity::vm::Value,
    ) -> Result<(u32, u32), GeneratorError> {
        let data = match value {
            clarity::vm::Value::Int(i) => {
                let mut data = (((*i as u128) & 0xFFFFFFFFFFFFFFFF) as i64)
                    .to_le_bytes()
                    .to_vec();
                data.extend_from_slice(&(((*i as u128) >> 64) as i64).to_le_bytes());
                data
            }
            clarity::vm::Value::UInt(u) => {
                let mut data = ((*u & 0xFFFFFFFFFFFFFFFF) as i64).to_le_bytes().to_vec();
                data.extend_from_slice(&((*u >> 64) as i64).to_le_bytes());
                data
            }
            clarity::vm::Value::Principal(p) => match p {
                PrincipalData::Standard(standard) => {
                    let mut data = vec![standard.0];
                    data.extend_from_slice(&standard.1);
                    // Append a 0 for the length of the contract name
                    data.push(0);
                    data
                }
                PrincipalData::Contract(contract) => {
                    let mut data = vec![contract.issuer.0];
                    data.extend_from_slice(&contract.issuer.1);
                    let contract_length = contract.name.len();
                    data.push(contract_length);
                    data.extend_from_slice(contract.name.as_bytes());
                    data
                }
            },
            clarity::vm::Value::Sequence(SequenceData::Buffer(buff_data)) => buff_data.data.clone(),
            clarity::vm::Value::Sequence(SequenceData::String(string_data)) => {
                return self.add_clarity_string_literal(string_data);
            }
            clarity::vm::Value::Bool(_)
            | clarity::vm::Value::Tuple(_)
            | clarity::vm::Value::Optional(_)
            | clarity::vm::Value::Response(_)
            | clarity::vm::Value::CallableContract(_)
            | clarity::vm::Value::Sequence(_) => {
                return Err(GeneratorError::TypeError(format!(
                    "Not a valid literal type: {:?}",
                    value
                )))
            }
        };
        let memory = self.get_memory()?;
        let offset = self.literal_memory_end;
        let len = data.len() as u32;
        self.module.data.add(
            DataKind::Active(ActiveData {
                memory,
                location: walrus::ActiveDataLocation::Absolute(offset),
            }),
            data.clone(),
        );
        self.literal_memory_end += data.len() as u32;

        Ok((offset, len))
    }

    pub(crate) fn block_from_expr(
        &mut self,
        builder: &mut InstrSeqBuilder,
        expr: &SymbolicExpression,
    ) -> Result<InstrSeqId, GeneratorError> {
        let return_type = clar2wasm_ty(self.get_expr_type(expr).ok_or_else(|| {
            GeneratorError::TypeError("Expression results must be typed".to_owned())
        })?);

        let mut block = builder.dangling_instr_seq(InstrSeqType::new(
            &mut self.module.types,
            &[],
            &return_type,
        ));
        self.traverse_expr(&mut block, expr)?;

        Ok(block.id())
    }

    /// Push a new local onto the call stack, adjusting the stack pointer and
    /// tracking the current function's frame size accordingly.
    /// - `include_repr` indicates if space should be reserved for the
    ///   representation of the value (e.g. the offset, length for an in-memory
    ///   type)
    /// - `include_value` indicates if space should be reserved for the value
    ///
    /// Returns a local which is a pointer to the beginning of the allocated
    /// stack space and the size of the allocated space.
    pub(crate) fn create_call_stack_local(
        &mut self,
        builder: &mut InstrSeqBuilder,
        ty: &TypeSignature,
        include_repr: bool,
        include_value: bool,
    ) -> (LocalId, i32) {
        let size = match (include_value, include_repr) {
            (true, true) => get_type_in_memory_size(ty, include_repr) + get_type_size(ty),
            (true, false) => get_type_in_memory_size(ty, include_repr),
            (false, true) => get_type_size(ty),
            (false, false) => unreachable!("must include either repr or value"),
        };

        // Save the offset (current stack pointer) into a local
        let offset = self.module.locals.add(ValType::I32);
        builder
            // []
            .global_get(self.stack_pointer)
            // [ stack_ptr ]
            .local_tee(offset);
        // [ stack_ptr ]

        // TODO: The frame stack size can be computed at compile time, so we
        //       should be able to increment the stack pointer once in the function
        //       prelude with a constant instead of incrementing it for each local.
        // (global.set $stack-pointer (i32.add (global.get $stack-pointer) (i32.const <size>))
        builder
            // [ stack_ptr ]
            .i32_const(size)
            // [ stack_ptr, size ]
            .binop(BinaryOp::I32Add)
            // [ new_stack_ptr ]
            .global_set(self.stack_pointer);
        // [  ]
        self.frame_size += size;

        (offset, size)
    }

    /// Write the value that is on the top of the data stack, which has type
    /// `ty`, to the memory, at offset stored in local variable,
    /// `offset_local`, plus constant offset `offset`. Returns the number of
    /// bytes written.
    pub(crate) fn write_to_memory(
        &mut self,
        builder: &mut InstrSeqBuilder,
        offset_local: LocalId,
        offset: u32,
        ty: &TypeSignature,
    ) -> Result<u32, GeneratorError> {
        let memory = self.get_memory()?;
        match ty {
            TypeSignature::IntType | TypeSignature::UIntType => {
                // Data stack: TOP | High | Low | ...
                // Save the high/low to locals.
                let high = self.module.locals.add(ValType::I64);
                let low = self.module.locals.add(ValType::I64);
                builder.local_set(high).local_set(low);

                // Store the high/low to memory.
                builder.local_get(offset_local).local_get(low).store(
                    memory,
                    StoreKind::I64 { atomic: false },
                    MemArg { align: 8, offset },
                );
                builder.local_get(offset_local).local_get(high).store(
                    memory,
                    StoreKind::I64 { atomic: false },
                    MemArg {
                        align: 8,
                        offset: offset + 8,
                    },
                );
                Ok(16)
            }
            TypeSignature::PrincipalType
            | TypeSignature::CallableType(_)
            | TypeSignature::TraitReferenceType(_)
            | TypeSignature::SequenceType(_) => {
                // Data stack: TOP | Length | Offset | ...
                // Save the offset/length to locals.
                let seq_offset = self.module.locals.add(ValType::I32);
                let seq_length = self.module.locals.add(ValType::I32);
                builder.local_set(seq_length).local_set(seq_offset);

                // Store the offset/length to memory.
                builder.local_get(offset_local).local_get(seq_offset).store(
                    memory,
                    StoreKind::I32 { atomic: false },
                    MemArg { align: 4, offset },
                );
                builder.local_get(offset_local).local_get(seq_length).store(
                    memory,
                    StoreKind::I32 { atomic: false },
                    MemArg {
                        align: 4,
                        offset: offset + 4,
                    },
                );
                Ok(8)
            }
            TypeSignature::BoolType => {
                // Data stack: TOP | Value | ...
                // Save the value to a local.
                let bool_val = self.module.locals.add(ValType::I32);
                builder.local_set(bool_val);

                // Store the value to memory.
                builder.local_get(offset_local).local_get(bool_val).store(
                    memory,
                    StoreKind::I32 { atomic: false },
                    MemArg { align: 4, offset },
                );
                Ok(4)
            }
            TypeSignature::NoType => {
                // Data stack: TOP | (Place holder i32)
                // We just have to drop the placeholder and write a i32
                builder.drop().local_get(offset_local).i32_const(0).store(
                    memory,
                    StoreKind::I32 { atomic: false },
                    MemArg { align: 4, offset },
                );
                Ok(4)
            }
            TypeSignature::OptionalType(some_ty) => {
                // Data stack: TOP | inner value | (some|none) variant
                // recursively store the inner value

                let bytes_written =
                    self.write_to_memory(builder, offset_local, offset + 4, some_ty)?;

                // Save the variant to a local and store it to memory
                let variant_val = self.module.locals.add(ValType::I32);
                builder
                    .local_set(variant_val)
                    .local_get(offset_local)
                    .local_get(variant_val)
                    .store(
                        memory,
                        StoreKind::I32 { atomic: false },
                        MemArg { align: 4, offset },
                    );

                // recursively store the inner value
                Ok(4 + bytes_written)
            }
            TypeSignature::ResponseType(ok_err_ty) => {
                // Data stack: TOP | err_value | ok_value | (ok|err) variant
                let mut bytes_written = 0;

                // write err value at offset + size of variant (4) + size of ok_value
                bytes_written += self.write_to_memory(
                    builder,
                    offset_local,
                    offset + 4 + get_type_size(&ok_err_ty.0) as u32,
                    &ok_err_ty.1,
                )?;

                // write ok value at offset + size of variant (4)
                bytes_written +=
                    self.write_to_memory(builder, offset_local, offset + 4, &ok_err_ty.0)?;

                let variant_val = self.module.locals.add(ValType::I32);
                builder
                    .local_set(variant_val)
                    .local_get(offset_local)
                    .local_get(variant_val)
                    .store(
                        memory,
                        StoreKind::I32 { atomic: false },
                        MemArg { align: 4, offset },
                    );

                Ok(bytes_written + 4)
            }
            TypeSignature::TupleType(tuple_ty) => {
                // Data stack: TOP | last_value | value_before_last | ... | first_value
                // we will write the values from last to first by setting the correct offset at which it's supposed to be written
                let mut bytes_written = 0;
                let types: Vec<_> = owned_ordered_tuple_signature(tuple_ty)
                    .into_values()
                    .collect();
                let offsets_delta: Vec<_> = std::iter::once(0u32)
                    .chain(
                        types
                            .iter()
                            .map(|t| get_type_size(t) as u32)
                            .scan(0, |acc, i| {
                                *acc += i;
                                Some(*acc)
                            }),
                    )
                    .collect();
                for (elem_ty, offset_delta) in types.into_iter().zip(offsets_delta).rev() {
                    bytes_written += self.write_to_memory(
                        builder,
                        offset_local,
                        offset + offset_delta,
                        &elem_ty,
                    )?;
                }
                Ok(bytes_written)
            }
            TypeSignature::ListUnionType(_) => Err(GeneratorError::TypeError(
                "Not a valid value type: ListUnionType".to_owned(),
            ))?,
        }
    }

    /// Read a value from memory at offset stored in local variable `offset`,
    /// with type `ty`, and push it onto the top of the data stack.
    pub(crate) fn read_from_memory(
        &mut self,
        builder: &mut InstrSeqBuilder,
        offset: LocalId,
        literal_offset: u32,
        ty: &TypeSignature,
    ) -> Result<i32, GeneratorError> {
        let memory = self
            .module
            .memories
            .iter()
            .next()
            .ok_or_else(|| GeneratorError::InternalError("No memory found".to_owned()))?;
        match ty {
            TypeSignature::IntType | TypeSignature::UIntType => {
                // Memory: Offset -> | Low | High |
                builder.local_get(offset).load(
                    memory.id(),
                    LoadKind::I64 { atomic: false },
                    MemArg {
                        align: 8,
                        offset: literal_offset,
                    },
                );
                builder.local_get(offset).load(
                    memory.id(),
                    LoadKind::I64 { atomic: false },
                    MemArg {
                        align: 8,
                        offset: literal_offset + 8,
                    },
                );
                Ok(16)
            }
            TypeSignature::OptionalType(inner) => {
                // Memory: Offset -> | Indicator | Value |
                builder.local_get(offset).load(
                    memory.id(),
                    LoadKind::I32 { atomic: false },
                    MemArg {
                        align: 4,
                        offset: literal_offset,
                    },
                );
                Ok(4 + self.read_from_memory(builder, offset, literal_offset + 4, inner)?)
            }
            TypeSignature::ResponseType(inner) => {
                // Memory: Offset -> | Indicator | Ok Value | Err Value |
                builder.local_get(offset).load(
                    memory.id(),
                    LoadKind::I32 { atomic: false },
                    MemArg {
                        align: 4,
                        offset: literal_offset,
                    },
                );
                let mut offset_adjust = 4;
                offset_adjust += self.read_from_memory(
                    builder,
                    offset,
                    literal_offset + offset_adjust,
                    &inner.0,
                )? as u32;
                offset_adjust += self.read_from_memory(
                    builder,
                    offset,
                    literal_offset + offset_adjust,
                    &inner.1,
                )? as u32;
                Ok(offset_adjust as i32)
            }
            // Principals and sequence types are stored in-memory and
            // represented by an offset and length.
            TypeSignature::PrincipalType
            | TypeSignature::CallableType(_)
            | TypeSignature::TraitReferenceType(_)
            | TypeSignature::SequenceType(_) => {
                // Memory: Offset -> | ValueOffset | ValueLength |
                builder.local_get(offset).load(
                    memory.id(),
                    LoadKind::I32 { atomic: false },
                    MemArg {
                        align: 4,
                        offset: literal_offset,
                    },
                );
                builder.local_get(offset).load(
                    memory.id(),
                    LoadKind::I32 { atomic: false },
                    MemArg {
                        align: 4,
                        offset: literal_offset + 4,
                    },
                );
                Ok(8)
            }
            TypeSignature::TupleType(tuple) => {
                // Memory: Offset -> | Value1 | Value2 | ... |
                let mut offset_adjust = 0;
                for &ty in ordered_tuple_signature(tuple).values() {
                    offset_adjust +=
                        self.read_from_memory(builder, offset, literal_offset + offset_adjust, ty)?
                            as u32;
                }
                Ok(offset_adjust as i32)
            }
            // Unknown types just get a placeholder i32 value.
            TypeSignature::NoType => {
                builder.i32_const(0);
                Ok(4)
            }
            TypeSignature::BoolType => {
                builder.local_get(offset).load(
                    memory.id(),
                    LoadKind::I32 { atomic: false },
                    MemArg {
                        align: 4,
                        offset: literal_offset,
                    },
                );
                Ok(4)
            }
            TypeSignature::ListUnionType(_) => Err(GeneratorError::TypeError(
                "Not a valid value type: ListUnionType".to_owned(),
            ))?,
        }
    }

    pub(crate) fn traverse_statement_list(
        &mut self,
        builder: &mut InstrSeqBuilder,
        statements: &[SymbolicExpression],
    ) -> Result<(), GeneratorError> {
        if statements.is_empty() {
            return Err(GeneratorError::InternalError(
                "statement list must have at least one statement".to_owned(),
            ));
        }

        let mut last_ty = None;
        // Traverse the statements, saving the last non-none value.
        for stmt in statements {
            // If stmt has a type, save that type. If there was a previous type
            // saved, then drop that value.
            if let Some(ty) = self.get_expr_type(stmt) {
                if let Some(last_ty) = &last_ty {
                    drop_value(builder.borrow_mut(), last_ty);
                }
                last_ty = Some(ty.clone());
            }
            self.traverse_expr(builder, stmt)?;
        }

        Ok(())
    }

    /// If `name` is a reserved variable, push its value onto the data stack.
    pub fn lookup_reserved_variable(
        &mut self,
        builder: &mut InstrSeqBuilder,
        name: &str,
        expr: &SymbolicExpression,
    ) -> Result<bool, GeneratorError> {
        if let Some(variable) = NativeVariables::lookup_by_name_at_version(
            name,
            &self.contract_analysis.clarity_version,
        ) {
            match variable {
                NativeVariables::TxSender => {
                    // Create a new local to hold the result on the call stack
                    let (offset, size);
                    (offset, size) = self.create_call_stack_local(
                        builder,
                        &TypeSignature::PrincipalType,
                        false,
                        true,
                    );

                    // Push the offset and size to the data stack
                    builder.local_get(offset).i32_const(size);

                    builder.call(self.func_by_name("stdlib.tx_sender"));

                    Ok(true)
                }
                NativeVariables::ContractCaller => {
                    // Create a new local to hold the result on the call stack
                    let (offset, size);
                    (offset, size) = self.create_call_stack_local(
                        builder,
                        &TypeSignature::PrincipalType,
                        false,
                        true,
                    );

                    // Push the offset and size to the data stack
                    builder.local_get(offset).i32_const(size);

                    // Call the host interface function, `contract_caller`
                    builder.call(self.func_by_name("stdlib.contract_caller"));
                    Ok(true)
                }
                NativeVariables::TxSponsor => {
                    // Create a new local to hold the result on the call stack
                    let (offset, size);
                    (offset, size) = self.create_call_stack_local(
                        builder,
                        &TypeSignature::PrincipalType,
                        false,
                        true,
                    );

                    // Push the offset and size to the data stack
                    builder.local_get(offset).i32_const(size);

                    // Call the host interface function, `tx_sponsor`

                    builder.call(self.func_by_name("stdlib.tx_sponsor"));
                    Ok(true)
                }
                NativeVariables::BlockHeight => {
                    // Call the host interface function, `block_height`
                    builder.call(self.func_by_name("stdlib.block_height"));
                    Ok(true)
                }
                NativeVariables::StacksBlockHeight => {
                    // Call the host interface function, `stacks_block_height`
                    builder.call(self.func_by_name("stdlib.stacks_block_height"));
                    Ok(true)
                }
                NativeVariables::TenureHeight => {
                    // Call the host interface function, `tenure_height`
                    builder.call(self.func_by_name("stdlib.tenure_height"));
                    Ok(true)
                }
                NativeVariables::BurnBlockHeight => {
                    // Call the host interface function, `burn_block_height`
                    builder.call(self.func_by_name("stdlib.burn_block_height"));
                    Ok(true)
                }
                NativeVariables::NativeNone => {
                    let ty = self.get_expr_type(expr).ok_or_else(|| {
                        GeneratorError::TypeError("'none' must be typed".to_owned())
                    })?;
                    add_placeholder_for_clarity_type(builder, ty);
                    Ok(true)
                }
                NativeVariables::NativeTrue => {
                    builder.i32_const(1);
                    Ok(true)
                }
                NativeVariables::NativeFalse => {
                    builder.i32_const(0);
                    Ok(true)
                }
                NativeVariables::TotalLiquidMicroSTX => {
                    // Call the host interface function, `stx_liquid_supply`
                    builder.call(self.func_by_name("stdlib.stx_liquid_supply"));
                    Ok(true)
                }
                NativeVariables::Regtest => {
                    // Call the host interface function, `is_in_regtest`
                    builder.call(self.func_by_name("stdlib.is_in_regtest"));
                    Ok(true)
                }
                NativeVariables::Mainnet => {
                    // Call the host interface function, `is_in_mainnet`
                    builder.call(self.func_by_name("stdlib.is_in_mainnet"));
                    Ok(true)
                }
                NativeVariables::ChainId => {
                    // Call the host interface function, `chain_id`
                    builder.call(self.func_by_name("stdlib.chain_id"));
                    Ok(true)
                }
            }
        } else {
            Ok(false)
        }
    }

    /// If `name` is a constant, push its value onto the data stack.
    pub fn lookup_constant_variable(
        &mut self,
        builder: &mut InstrSeqBuilder,
        name: &str,
        expr: &SymbolicExpression,
    ) -> Result<bool, GeneratorError> {
        if let Some(offset) = self.constants.get(name) {
            // Load the offset into a local variable
            let offset_local = self.module.locals.add(ValType::I32);
            builder.i32_const(*offset as i32).local_set(offset_local);

            let ty = self
                .get_expr_type(expr)
                .ok_or_else(|| GeneratorError::TypeError("constant must be typed".to_owned()))?
                .clone();

            self.read_from_memory(builder, offset_local, 0, &ty)?;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    /// Save the expression on the top of the stack, with Clarity type `ty`, to
    /// local variables. If `fix_ordering` is true, then the vector is reversed
    /// so that the types are in logical order. Without this, they will be in
    /// reverse order, due to the order we pop values from the stack. Return
    /// the list of local variables.
    pub fn save_to_locals(
        &mut self,
        builder: &mut walrus::InstrSeqBuilder,
        ty: &TypeSignature,
        fix_ordering: bool,
    ) -> Vec<LocalId> {
        let wasm_types = clar2wasm_ty(ty);
        let mut locals = Vec::with_capacity(wasm_types.len());
        // Iterate in reverse order, since we are popping items off of the top
        // in reverse order.
        for wasm_ty in wasm_types.iter().rev() {
            let local = self.module.locals.add(*wasm_ty);
            locals.push(local);
            builder.local_set(local);
        }

        if fix_ordering {
            // Reverse the locals to put them back in the correct order.
            locals.reverse();
        }
        locals
    }

    pub fn func_by_name(&self, name: &str) -> FunctionId {
        self.module
            .funcs
            .by_name(name)
            .unwrap_or_else(|| panic!("function not found: {name}"))
    }

    pub fn get_function_type(&self, name: &str) -> Option<&FunctionType> {
        let analysis = &self.contract_analysis;

        analysis
            .get_public_function_type(name)
            .or(analysis.get_read_only_function_type(name))
            .or(analysis.get_private_function(name))
    }

    fn visit_literal_value(
        &mut self,
        builder: &mut InstrSeqBuilder,
        _expr: &SymbolicExpression,
        value: &clarity::vm::Value,
    ) -> Result<(), GeneratorError> {
        match value {
            clarity::vm::Value::Int(i) => {
                builder.i64_const((i & 0xFFFFFFFFFFFFFFFF) as i64);
                builder.i64_const(((i >> 64) & 0xFFFFFFFFFFFFFFFF) as i64);
                Ok(())
            }
            clarity::vm::Value::UInt(u) => {
                builder.i64_const((u & 0xFFFFFFFFFFFFFFFF) as i64);
                builder.i64_const(((u >> 64) & 0xFFFFFFFFFFFFFFFF) as i64);
                Ok(())
            }
            clarity::vm::Value::Sequence(SequenceData::String(s)) => {
                let (offset, len) = self.add_clarity_string_literal(s)?;
                builder.i32_const(offset as i32);
                builder.i32_const(len as i32);
                Ok(())
            }
            clarity::vm::Value::Principal(_)
            | clarity::vm::Value::Sequence(SequenceData::Buffer(_)) => {
                let (offset, len) = self.add_literal(value)?;
                builder.i32_const(offset as i32);
                builder.i32_const(len as i32);
                Ok(())
            }
            clarity::vm::Value::Bool(_)
            | clarity::vm::Value::Tuple(_)
            | clarity::vm::Value::Optional(_)
            | clarity::vm::Value::Response(_)
            | clarity::vm::Value::CallableContract(_)
            | clarity::vm::Value::Sequence(_) => Err(GeneratorError::TypeError(format!(
                "Not a valid literal type: {:?}",
                value
            ))),
        }
    }

    fn visit_atom(
        &mut self,
        builder: &mut InstrSeqBuilder,
        expr: &SymbolicExpression,
        atom: &ClarityName,
    ) -> Result<(), GeneratorError> {
        // Handle builtin variables
        if self.lookup_reserved_variable(builder, atom.as_str(), expr)? {
            return Ok(());
        }

        if self.lookup_constant_variable(builder, atom.as_str(), expr)? {
            return Ok(());
        }

        // Handle parameters and local bindings
        let values = self.bindings.get(atom.as_str()).ok_or_else(|| {
            GeneratorError::InternalError(format!("unable to find local for {}", atom.as_str()))
        })?;

        for value in values {
            builder.local_get(*value);
        }

        Ok(())
    }

    fn traverse_call_user_defined(
        &mut self,
        builder: &mut InstrSeqBuilder,
        expr: &SymbolicExpression,
        name: &ClarityName,
        args: &[SymbolicExpression],
    ) -> Result<(), GeneratorError> {
        // WORKAROUND: the typechecker in epoch < 2.1 fails to set correct types for functions
        //             arguments. We set them ourselves. We don't make the distinction between
        //             epochs since it would require a deeper modification and it doesn't impact
        //             the newer ones.
        if let Some(FunctionType::Fixed(FixedFunction {
            args: function_args,
            ..
        })) = self.get_function_type(name)
        {
            for (arg, signature) in args
                .iter()
                .zip(function_args.clone().into_iter().map(|a| a.signature))
            {
                self.set_expr_type(arg, signature)?;
            }
        }
        self.traverse_args(builder, args)?;

        let return_ty = self
            .get_expr_type(expr)
            .ok_or_else(|| {
                GeneratorError::TypeError("function call expression must be typed".to_owned())
            })?
            .clone();
        self.visit_call_user_defined(builder, &return_ty, name)
    }

    /// Visit a function call to a user-defined function. Arguments must have
    /// already been traversed and pushed to the stack.
    pub fn visit_call_user_defined(
        &mut self,
        builder: &mut InstrSeqBuilder,
        return_ty: &TypeSignature,
        name: &ClarityName,
    ) -> Result<(), GeneratorError> {
        if self
            .contract_analysis
            .get_public_function_type(name.as_str())
            .is_some()
        {
            self.local_call_public(builder, return_ty, name)?;
        } else if self
            .contract_analysis
            .get_read_only_function_type(name.as_str())
            .is_some()
        {
            self.local_call_read_only(builder, name)?;
        } else if self
            .contract_analysis
            .get_private_function(name.as_str())
            .is_some()
        {
            self.local_call(builder, name)?;
        } else {
            return Err(GeneratorError::TypeError(format!(
                "function not found: {name}",
                name = name.as_str()
            )));
        }

        // If an in-memory value is returned from a function, we need to copy
        // it to our frame, from the callee's frame.
        if is_in_memory_type(return_ty) {
            // The result may be in the callee's call frame, can be overwritten
            // after returning, so we need to copy it to our frame.
            let result_offset = self.module.locals.add(ValType::I32);
            let result_length = self.module.locals.add(ValType::I32);
            builder.local_set(result_length).local_set(result_offset);

            // Reserve space to store the returned value.
            let offset = self.module.locals.add(ValType::I32);
            builder.global_get(self.stack_pointer).local_tee(offset);
            builder
                .local_get(result_length)
                .binop(BinaryOp::I32Add)
                .global_set(self.stack_pointer);

            let memory = self.get_memory()?;

            // Copy the result to our frame.
            builder
                .local_get(offset)
                .local_get(result_offset)
                .local_get(result_length)
                .memory_copy(memory, memory);

            // Push the copied offset and length to the stack
            builder.local_get(offset).local_get(result_length);
        }

        Ok(())
    }

    /// Call a function defined in the current contract.
    fn local_call(
        &mut self,
        builder: &mut InstrSeqBuilder,
        name: &ClarityName,
    ) -> Result<(), GeneratorError> {
        builder.call(self.func_by_name(name.as_str()));

        Ok(())
    }

    /// Call a public function defined in the current contract. This requires
    /// going through the host interface to handle roll backs.
    fn local_call_public(
        &mut self,
        builder: &mut InstrSeqBuilder,
        return_ty: &TypeSignature,
        name: &ClarityName,
    ) -> Result<(), GeneratorError> {
        // Call the host interface function, `begin_public_call`
        builder.call(self.func_by_name("stdlib.begin_public_call"));

        self.local_call(builder, name)?;

        // Save the result to a local
        let result_locals = self.save_to_locals(builder, return_ty, true);

        // If the result is an `ok`, then we can commit the call, and if it
        // is an `err`, then we roll it back. `result_locals[0]` is the
        // response indicator (all public functions return a response).
        let if_id = {
            let mut if_case: InstrSeqBuilder<'_> = builder.dangling_instr_seq(None);
            if_case.call(self.func_by_name("stdlib.commit_call"));
            if_case.id()
        };

        let else_id = {
            let mut else_case: InstrSeqBuilder<'_> = builder.dangling_instr_seq(None);
            else_case.call(self.func_by_name("stdlib.roll_back_call"));
            else_case.id()
        };

        builder.local_get(result_locals[0]).instr(IfElse {
            consequent: if_id,
            alternative: else_id,
        });

        // Restore the result to the top of the stack.
        for local in &result_locals {
            builder.local_get(*local);
        }

        Ok(())
    }

    /// Call a read-only function defined in the current contract.
    fn local_call_read_only(
        &mut self,
        builder: &mut InstrSeqBuilder,
        name: &ClarityName,
    ) -> Result<(), GeneratorError> {
        // Call the host interface function, `begin_readonly_call`
        builder.call(self.func_by_name("stdlib.begin_read_only_call"));

        self.local_call(builder, name)?;

        // Call the host interface function, `roll_back_call`
        builder.call(self.func_by_name("stdlib.roll_back_call"));

        Ok(())
    }

    pub fn traverse_args(
        &mut self,
        builder: &mut InstrSeqBuilder,
        args: &[SymbolicExpression],
    ) -> Result<(), GeneratorError> {
        for arg in args.iter() {
            self.traverse_expr(builder, arg)?;
        }
        Ok(())
    }

    #[allow(dead_code)]
    /// Log an i64 that is on top of the stack.
    pub fn debug_log_i64(&self, builder: &mut InstrSeqBuilder) {
        builder.call(self.func_by_name("log"));
    }

    #[allow(dead_code)]
    /// Log an i32 that is on top of the stack.
    pub fn debug_log_i32(&self, builder: &mut InstrSeqBuilder) {
        builder
            .unop(UnaryOp::I64ExtendUI32)
            .call(self.func_by_name("log"));
    }

    pub(crate) fn is_reserved_name(&self, name: &ClarityName) -> bool {
        let version = self.contract_analysis.clarity_version;

        functions::lookup_reserved_functions(name.as_str(), &version).is_some()
            || variables::is_reserved_name(name, &version)
    }

    pub fn get_sequence_element_type(
        &self,
        sequence: &SymbolicExpression,
    ) -> Result<SequenceElementType, GeneratorError> {
        match self.get_expr_type(sequence).ok_or_else(|| {
            GeneratorError::TypeError("sequence expression must be typed".to_owned())
        })? {
            TypeSignature::SequenceType(seq_ty) => match &seq_ty {
                SequenceSubtype::ListType(list_type) => Ok(SequenceElementType::Other(
                    list_type.get_list_item_type().clone(),
                )),
                SequenceSubtype::BufferType(_)
                | SequenceSubtype::StringType(StringSubtype::ASCII(_)) => {
                    // For buffer and string-ascii return none, which indicates
                    // that elements should be read byte-by-byte.
                    Ok(SequenceElementType::Byte)
                }
                SequenceSubtype::StringType(StringSubtype::UTF8(_)) => {
                    Ok(SequenceElementType::UnicodeScalar)
                }
            },
            _ => Err(GeneratorError::TypeError(
                "expected sequence type".to_string(),
            )),
        }
    }
}

#[cfg(test)]
mod misc_tests {
    use std::env;

    use walrus::Module;

    // Tests that don't relate to specific words
    use crate::{
        tools::{crosscheck, evaluate},
        wasm_generator::END_OF_STANDARD_DATA,
    };

    #[test]
    fn is_in_mainnet() {
        crosscheck(
            "
(define-public (mainnet)
  (ok is-in-mainnet))

(mainnet)
",
            evaluate("(ok false)"),
        );
    }

    #[test]
    fn is_in_regtest() {
        crosscheck(
            "
(define-public (regtest)
  (ok is-in-regtest))

(regtest)
",
            evaluate("(ok false)"),
        );
    }

    #[test]
    fn should_set_memory_pages() {
        let string_size = 262000;
        let a = "a".repeat(string_size);
        let b = "b".repeat(string_size);
        let c = "c".repeat(string_size);
        let d = "d".repeat(string_size);

        let snippet = format!("(is-eq u\"{}\" u\"{}\" u\"{}\" u\"{}\")", a, b, c, d);
        crosscheck(&snippet, Ok(Some(clarity::vm::Value::Bool(false))));
    }

    #[test]
    fn end_of_standard_data_is_correct() {
        const STANDARD_LIB_PATH: &str =
            concat!(env!("CARGO_MANIFEST_DIR"), "/src/standard/standard.wasm");
        let standard_lib_wasm = std::fs::read(STANDARD_LIB_PATH).expect("Failed to read WASM file");
        let module = Module::from_buffer(&standard_lib_wasm).unwrap();
        let initial_data_size: usize = module.data.iter().map(|d| d.value.len()).sum();

        assert!((initial_data_size as u32) == END_OF_STANDARD_DATA);
    }

    #[test]
    fn function_argument_have_correct_type() {
        let snippet = r#"
            (define-private (foo (arg (optional uint)))
                true
            )

            (foo none)
        "#;
        crosscheck(snippet, Ok(Some(clarity::vm::Value::Bool(true))));

        // issue 340 showed a bug for epoch < 2.1
        assert!(crate::tools::evaluate_at(
            snippet,
            clarity::types::StacksEpochId::Epoch20,
            clarity::vm::version::ClarityVersion::latest(),
        )
        .is_ok());
    }

    #[test]
    fn top_level_result_none() {
        crosscheck(
            "
(define-public (foo)
  (ok true))

(define-public (bar)
  (ok true))
",
            Ok(None),
        );
    }

    #[test]
    fn top_level_result_some_last() {
        crosscheck(
            "
(define-private (foo) 42)
(define-public (bar)
  (ok true))
(foo)
",
            evaluate("42"),
        );
    }

    #[test]
    fn top_level_result_some_not_last() {
        crosscheck(
            "
(define-public (foo)
  (ok true))
(foo)
(define-public (bar)
  (ok true))
",
            evaluate("(ok true)"),
        );
    }
}
