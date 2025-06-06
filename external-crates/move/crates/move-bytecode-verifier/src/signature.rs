// Copyright (c) The Diem Core Contributors
// Copyright (c) The Move Contributors
// SPDX-License-Identifier: Apache-2.0

//! This module implements a checker for verifying signature tokens used in types of function
//! parameters, locals, and fields of structs are well-formed. References can only occur at the
//! top-level in all tokens.  Additionally, references cannot occur at all in field types.
use move_binary_format::{
    IndexKind,
    errors::{Location, PartialVMError, PartialVMResult, VMResult},
    file_format::{
        AbilitySet, Bytecode, CodeUnit, CompiledModule, DatatypeTyParameter, EnumDefinition,
        FunctionDefinition, FunctionHandle, Signature, SignatureIndex, SignatureToken,
        StructDefinition, StructFieldInformation, TableIndex,
    },
    file_format_common::VERSION_6,
};
use move_bytecode_verifier_meter::{Meter, Scope};
use move_core_types::vm_status::StatusCode;
use std::collections::{HashMap, HashSet};

use crate::ability_cache::AbilityCache;

pub struct SignatureChecker<'env, 'a, 'b, M: Meter + ?Sized> {
    module: &'env CompiledModule,
    module_ability_cache: &'a mut AbilityCache<'env>,
    meter: &'b mut M,
    abilities_cache: HashMap<SignatureIndex, HashSet<Vec<AbilitySet>>>,
}

impl<'env, 'a, 'b, M: Meter + ?Sized> SignatureChecker<'env, 'a, 'b, M> {
    pub fn verify_module(
        module: &'env CompiledModule,
        module_ability_cache: &'a mut AbilityCache<'env>,
        meter: &'b mut M,
    ) -> VMResult<()> {
        Self::verify_module_impl(module, module_ability_cache, meter)
            .map_err(|e| e.finish(Location::Module(module.self_id())))
    }

    fn verify_module_impl(
        module: &'env CompiledModule,
        module_ability_cache: &'a mut AbilityCache<'env>,
        meter: &'b mut M,
    ) -> PartialVMResult<()> {
        let mut sig_check = Self {
            module,
            module_ability_cache,
            meter,
            abilities_cache: HashMap::new(),
        };
        sig_check.verify_signature_pool(module.signatures())?;
        sig_check.verify_function_signatures(module.function_handles())?;
        sig_check.verify_struct_fields(module.struct_defs())?;
        sig_check.verify_enum_fields(module.enum_defs())?;
        sig_check.verify_code_units(module.function_handles(), module.function_defs())
    }

    fn verify_signature_pool(&self, signatures: &[Signature]) -> PartialVMResult<()> {
        for i in 0..signatures.len() {
            self.check_signature(SignatureIndex::new(i as TableIndex))?
        }
        Ok(())
    }

    fn verify_function_signatures(
        &mut self,
        function_handles: &[FunctionHandle],
    ) -> PartialVMResult<()> {
        let err_handler = |err: PartialVMError, idx| {
            err.at_index(IndexKind::Signature, idx as TableIndex)
                .at_index(IndexKind::FunctionHandle, idx as TableIndex)
        };

        for (idx, fh) in function_handles.iter().enumerate() {
            self.check_instantiation(fh.return_, &fh.type_parameters)
                .map_err(|err| err_handler(err, idx))?;
            self.check_instantiation(fh.parameters, &fh.type_parameters)
                .map_err(|err| err_handler(err, idx))?;
            if !fh.type_parameters.is_empty() {}
        }
        Ok(())
    }

    fn verify_struct_fields(&mut self, struct_defs: &[StructDefinition]) -> PartialVMResult<()> {
        for (struct_def_idx, struct_def) in struct_defs.iter().enumerate() {
            let fields = match &struct_def.field_information {
                StructFieldInformation::Native => continue,
                StructFieldInformation::Declared(fields) => fields,
            };
            let struct_handle = self.module.datatype_handle_at(struct_def.struct_handle);
            let type_param_constraints: Vec<_> = struct_handle.type_param_constraints().collect();
            let err_handler = |err: PartialVMError, idx| {
                err.at_index(IndexKind::FieldDefinition, idx as TableIndex)
                    .at_index(IndexKind::StructDefinition, struct_def_idx as TableIndex)
            };
            for (field_offset, field_def) in fields.iter().enumerate() {
                self.check_signature_token(&field_def.signature.0)
                    .map_err(|err| err_handler(err, field_offset))?;
                self.check_type_instantiation(&field_def.signature.0, &type_param_constraints)
                    .map_err(|err| err_handler(err, field_offset))?;

                self.check_phantom_params(
                    &field_def.signature.0,
                    false,
                    &struct_handle.type_parameters,
                )
                .map_err(|err| err_handler(err, field_offset))?;
            }
        }
        Ok(())
    }

    fn verify_enum_fields(&mut self, enum_defs: &[EnumDefinition]) -> PartialVMResult<()> {
        for (enum_def_idx, enum_def) in enum_defs.iter().enumerate() {
            let enum_handle = self.module.datatype_handle_at(enum_def.enum_handle);
            let type_param_constraints: Vec<_> = enum_handle.type_param_constraints().collect();
            let err_handler = |err: PartialVMError, v_idx, f_idx| {
                err.at_index(IndexKind::FieldDefinition, f_idx as TableIndex)
                    .at_index(IndexKind::VariantTag, v_idx as TableIndex)
                    .at_index(IndexKind::EnumDefinition, enum_def_idx as TableIndex)
            };
            for (tag, variant) in enum_def.variants.iter().enumerate() {
                for (field_idx, field_def) in variant.fields.iter().enumerate() {
                    self.check_signature_token(&field_def.signature.0)
                        .map_err(|err| err_handler(err, tag, field_idx))?;
                    self.check_type_instantiation(&field_def.signature.0, &type_param_constraints)
                        .map_err(|err| err_handler(err, tag, field_idx))?;

                    self.check_phantom_params(
                        &field_def.signature.0,
                        false,
                        &enum_handle.type_parameters,
                    )
                    .map_err(|err| err_handler(err, tag, field_idx))?;
                }
            }
        }
        Ok(())
    }

    fn verify_code_units(
        &mut self,
        function_handles: &[FunctionHandle],
        function_defs: &[FunctionDefinition],
    ) -> PartialVMResult<()> {
        for (func_def_idx, func_def) in function_defs.iter().enumerate() {
            // skip native functions
            let code = match &func_def.code {
                Some(code) => code,
                None => continue,
            };
            let func_handle = &function_handles[func_def.function.0 as usize];
            self.verify_code(code, &func_handle.type_parameters)
                .map_err(|err| {
                    err.at_index(IndexKind::Signature, code.locals.0)
                        .at_index(IndexKind::FunctionDefinition, func_def_idx as TableIndex)
                })?
        }
        Ok(())
    }

    fn verify_code(
        &mut self,
        code: &CodeUnit,
        type_parameters: &[AbilitySet],
    ) -> PartialVMResult<()> {
        self.check_instantiation(code.locals, type_parameters)?;

        // Check if the type actuals in certain bytecode instructions are well defined.
        use Bytecode::*;
        for (offset, instr) in code.code.iter().enumerate() {
            let result = match instr {
                CallGeneric(idx) => {
                    let func_inst = self.module.function_instantiation_at(*idx);
                    let func_handle = self.module.function_handle_at(func_inst.handle);
                    let type_arguments = &self.module.signature_at(func_inst.type_parameters).0;
                    self.check_signature_tokens(type_arguments)?;
                    self.check_generic_instance(
                        type_arguments,
                        func_handle.type_parameters.iter().copied(),
                        type_parameters,
                    )
                }
                PackGeneric(idx)
                | UnpackGeneric(idx)
                | ExistsGenericDeprecated(idx)
                | MoveFromGenericDeprecated(idx)
                | MoveToGenericDeprecated(idx)
                | ImmBorrowGlobalGenericDeprecated(idx)
                | MutBorrowGlobalGenericDeprecated(idx) => {
                    let struct_inst = self.module.struct_instantiation_at(*idx);
                    let struct_def = self.module.struct_def_at(struct_inst.def);
                    let struct_handle = self.module.datatype_handle_at(struct_def.struct_handle);
                    let type_arguments = &self.module.signature_at(struct_inst.type_parameters).0;
                    self.check_signature_tokens(type_arguments)?;
                    self.check_generic_instance(
                        type_arguments,
                        struct_handle.type_param_constraints(),
                        type_parameters,
                    )
                }
                ImmBorrowFieldGeneric(idx) | MutBorrowFieldGeneric(idx) => {
                    let field_inst = self.module.field_instantiation_at(*idx);
                    let field_handle = self.module.field_handle_at(field_inst.handle);
                    let struct_def = self.module.struct_def_at(field_handle.owner);
                    let struct_handle = self.module.datatype_handle_at(struct_def.struct_handle);
                    let type_arguments = &self.module.signature_at(field_inst.type_parameters).0;
                    self.check_signature_tokens(type_arguments)?;
                    self.check_generic_instance(
                        type_arguments,
                        struct_handle.type_param_constraints(),
                        type_parameters,
                    )
                }
                VecPack(idx, _)
                | VecLen(idx)
                | VecImmBorrow(idx)
                | VecMutBorrow(idx)
                | VecPushBack(idx)
                | VecPopBack(idx)
                | VecUnpack(idx, _)
                | VecSwap(idx) => {
                    let type_arguments = &self.module.signature_at(*idx).0;
                    if type_arguments.len() != 1 {
                        return Err(PartialVMError::new(
                            StatusCode::NUMBER_OF_TYPE_ARGUMENTS_MISMATCH,
                        )
                        .with_message(format!(
                            "expected 1 type token for vector operations, got {}",
                            type_arguments.len()
                        )));
                    }
                    self.check_signature_tokens(type_arguments)
                }

                PackVariantGeneric(vidx)
                | UnpackVariantGeneric(vidx)
                | UnpackVariantGenericImmRef(vidx)
                | UnpackVariantGenericMutRef(vidx) => {
                    let handle = self.module.variant_instantiation_handle_at(*vidx);
                    let enum_inst = self.module.enum_instantiation_at(handle.enum_def);
                    let enum_def = self.module.enum_def_at(enum_inst.def);
                    let enum_handle = self.module.datatype_handle_at(enum_def.enum_handle);
                    let type_arguments = &self.module.signature_at(enum_inst.type_parameters).0;
                    self.check_signature_tokens(type_arguments)?;
                    self.check_generic_instance(
                        type_arguments,
                        enum_handle.type_param_constraints(),
                        type_parameters,
                    )
                }

                // List out the other options explicitly so there's a compile error if a new
                // bytecode gets added.
                Pop
                | Ret
                | Branch(_)
                | BrTrue(_)
                | BrFalse(_)
                | LdU8(_)
                | LdU16(_)
                | LdU32(_)
                | LdU64(_)
                | LdU128(_)
                | LdU256(_)
                | LdConst(_)
                | CastU8
                | CastU16
                | CastU32
                | CastU64
                | CastU128
                | CastU256
                | LdTrue
                | LdFalse
                | Call(_)
                | Pack(_)
                | Unpack(_)
                | ReadRef
                | WriteRef
                | FreezeRef
                | Add
                | Sub
                | Mul
                | Mod
                | Div
                | BitOr
                | BitAnd
                | Xor
                | Shl
                | Shr
                | Or
                | And
                | Not
                | Eq
                | Neq
                | Lt
                | Gt
                | Le
                | Ge
                | CopyLoc(_)
                | MoveLoc(_)
                | StLoc(_)
                | MutBorrowLoc(_)
                | ImmBorrowLoc(_)
                | MutBorrowField(_)
                | ImmBorrowField(_)
                | MutBorrowGlobalDeprecated(_)
                | ImmBorrowGlobalDeprecated(_)
                | ExistsDeprecated(_)
                | MoveToDeprecated(_)
                | MoveFromDeprecated(_)
                | Abort
                | Nop
                | VariantSwitch(_)
                | PackVariant(_)
                | UnpackVariant(_)
                | UnpackVariantImmRef(_)
                | UnpackVariantMutRef(_) => Ok(()),
            };
            result.map_err(|err| {
                err.append_message_with_separator(' ', format!("at offset {} ", offset))
            })?
        }
        Ok(())
    }

    /// Checks that phantom type parameters are only used in phantom position.
    fn check_phantom_params(
        &self,
        ty: &SignatureToken,
        is_phantom_pos: bool,
        type_parameters: &[DatatypeTyParameter],
    ) -> PartialVMResult<()> {
        match ty {
            SignatureToken::Vector(ty) => self.check_phantom_params(ty, false, type_parameters)?,
            SignatureToken::DatatypeInstantiation(inst) => {
                let (idx, type_arguments) = &**inst;
                let sh = self.module.datatype_handle_at(*idx);
                for (i, ty) in type_arguments.iter().enumerate() {
                    self.check_phantom_params(
                        ty,
                        sh.type_parameters[i].is_phantom,
                        type_parameters,
                    )?;
                }
            }
            SignatureToken::TypeParameter(idx) => {
                if type_parameters[*idx as usize].is_phantom && !is_phantom_pos {
                    return Err(PartialVMError::new(
                        StatusCode::INVALID_PHANTOM_TYPE_PARAM_POSITION,
                    )
                    .with_message(
                        "phantom type parameter cannot be used in non-phantom position".to_string(),
                    ));
                }
            }

            SignatureToken::Datatype(_)
            | SignatureToken::Reference(_)
            | SignatureToken::MutableReference(_)
            | SignatureToken::Bool
            | SignatureToken::U8
            | SignatureToken::U16
            | SignatureToken::U32
            | SignatureToken::U64
            | SignatureToken::U128
            | SignatureToken::U256
            | SignatureToken::Address
            | SignatureToken::Signer => {}
        }
        Ok(())
    }

    /// Checks if the given type is well defined in the given context.
    /// References are only permitted at the top level.
    fn check_signature(&self, idx: SignatureIndex) -> PartialVMResult<()> {
        for token in &self.module.signature_at(idx).0 {
            match token {
                SignatureToken::Reference(inner) | SignatureToken::MutableReference(inner) => {
                    self.check_signature_token(inner)?
                }
                _ => self.check_signature_token(token)?,
            }
        }
        Ok(())
    }

    /// Checks if the given types are well defined in the given context.
    /// No references are permitted.
    fn check_signature_tokens(&self, tys: &[SignatureToken]) -> PartialVMResult<()> {
        for ty in tys {
            self.check_signature_token(ty)?
        }
        Ok(())
    }

    /// Checks if the given type is well defined in the given context.
    /// No references are permitted.
    fn check_signature_token(&self, ty: &SignatureToken) -> PartialVMResult<()> {
        use SignatureToken::*;
        match ty {
            U8 | U16 | U32 | U64 | U128 | U256 | Bool | Address | Signer | Datatype(_)
            | TypeParameter(_) => Ok(()),
            Reference(_) | MutableReference(_) => {
                // TODO: Prop tests expect us to NOT check the inner types.
                // Revisit this once we rework prop tests.
                Err(PartialVMError::new(StatusCode::INVALID_SIGNATURE_TOKEN)
                    .with_message("reference not allowed".to_string()))
            }
            Vector(ty) => self.check_signature_token(ty),
            DatatypeInstantiation(inst) => {
                let (_, type_arguments) = &**inst;
                self.check_signature_tokens(type_arguments)
            }
        }
    }

    fn check_instantiation(
        &mut self,
        idx: SignatureIndex,
        type_parameters: &[AbilitySet],
    ) -> PartialVMResult<()> {
        if let Some(checked_abilities) = self.abilities_cache.get(&idx) {
            if checked_abilities.contains(type_parameters) {
                return Ok(());
            }
        };
        for ty in &self.module.signature_at(idx).0 {
            self.check_type_instantiation(ty, type_parameters)?
        }
        let checked_abilities = self.abilities_cache.entry(idx).or_default();
        checked_abilities.insert(type_parameters.to_vec());
        Ok(())
    }

    fn check_type_instantiation(
        &mut self,
        s: &SignatureToken,
        type_parameters: &[AbilitySet],
    ) -> PartialVMResult<()> {
        if self.module.version() >= VERSION_6 {
            for ty in s.preorder_traversal() {
                self.check_type_instantiation_(ty, type_parameters)?
            }
            Ok(())
        } else {
            // preserve buggy, but harmless old behavior for backward compatibility
            self.check_type_instantiation_(s, type_parameters)
        }
    }

    fn check_type_instantiation_(
        &mut self,
        s: &SignatureToken,
        type_parameters: &[AbilitySet],
    ) -> PartialVMResult<()> {
        match s {
            SignatureToken::DatatypeInstantiation(inst) => {
                let (idx, type_arguments) = &**inst;
                // Check that the instantiation satisfies the `idx` struct's constraints
                // Cannot be checked completely if we do not know the constraints of type parameters
                // i.e. it cannot be checked unless we are inside some module member. The only case
                // where that happens is when checking the signature pool itself
                let sh = self.module.datatype_handle_at(*idx);
                self.check_generic_instance(
                    type_arguments,
                    sh.type_param_constraints(),
                    type_parameters,
                )
            }
            SignatureToken::Reference(_)
            | SignatureToken::MutableReference(_)
            | SignatureToken::Vector(_)
            | SignatureToken::TypeParameter(_)
            | SignatureToken::Datatype(_)
            | SignatureToken::Bool
            | SignatureToken::U8
            | SignatureToken::U16
            | SignatureToken::U32
            | SignatureToken::U64
            | SignatureToken::U128
            | SignatureToken::U256
            | SignatureToken::Address
            | SignatureToken::Signer => Ok(()),
        }
    }

    // Checks if the given types are well defined and satisfy the constraints in the given context.
    fn check_generic_instance(
        &mut self,
        type_arguments: &[SignatureToken],
        constraints: impl ExactSizeIterator<Item = AbilitySet>,
        global_abilities: &[AbilitySet],
    ) -> PartialVMResult<()> {
        if type_arguments.len() != constraints.len() {
            return Err(
                PartialVMError::new(StatusCode::NUMBER_OF_TYPE_ARGUMENTS_MISMATCH).with_message(
                    format!(
                        "expected {} type argument(s), got {}",
                        constraints.len(),
                        type_arguments.len()
                    ),
                ),
            );
        }

        let meter: &mut M = self.meter;
        let module_ability_cache: &mut AbilityCache = self.module_ability_cache;
        for (constraint, ty) in constraints.into_iter().zip(type_arguments) {
            let given =
                module_ability_cache.abilities(Scope::Module, meter, global_abilities, ty)?;
            if !constraint.is_subset(given) {
                return Err(PartialVMError::new(StatusCode::CONSTRAINT_NOT_SATISFIED)
                    .with_message(format!(
                        "expected type with abilities {:?} got type actual {:?} with incompatible \
                        abilities {:?}",
                        constraint, ty, given
                    )));
            }
        }
        Ok(())
    }
}
