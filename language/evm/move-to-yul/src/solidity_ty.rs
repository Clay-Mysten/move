// Copyright (c) The Diem Core Contributors
// SPDX-License-Identifier: Apache-2.0

//! Representation of solidity types and related functions.
//! TODO: struct and function type

use anyhow::{anyhow, Context as AnyhowContext};
use itertools::Itertools;
use once_cell::sync::Lazy;
use regex::Regex;
use std::{fmt, fmt::Formatter};

use move_model::{
    model::FunctionEnv,
    ty::{PrimitiveType, Type},
};

use crate::context::Context;

const PARSE_ERR_MSG: &str = "error happens when parsing the signature";
const PARSE_ERR_MSG_SIMPLE_TYPE: &str = "error happens when parsing a simple type";
const PARSE_ERR_MSG_ARRAY_TYPE: &str = "error happens when parsing an array type";
const PARSE_ERR_MSG_RETURN: &str = "error happens when parsing the return types in the signature";

/// Represents a Solidity Signature appearing in the callable attribute.
#[derive(Debug, Clone)]
pub(crate) struct SoliditySignature {
    pub sig_name: String,
    pub para_types: Vec<(SolidityType, SignatureDataLocation)>,
    pub ret_types: Vec<(SolidityType, SignatureDataLocation)>,
}

#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Clone)]
pub(crate) enum SignatureDataLocation {
    // CallData, calldata is not supported yet
    Memory,
}

/// Represents a primitive value type.
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Clone, Copy)]
pub(crate) enum SolidityPrimitiveType {
    Bool,
    Uint(usize),
    Int(usize),
    Fixed(usize, usize),
    Ufixed(usize, usize),
    Address(bool),
}

/// Represents a Solidity type
/// TODO: struct
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Clone)]
pub(crate) enum SolidityType {
    Primitive(SolidityPrimitiveType),
    Tuple(Vec<SolidityType>),
    DynamicArray(Box<SolidityType>),
    StaticArray(Box<SolidityType>, usize),
    SolidityString,
    Bytes,
    BytesStatic(usize),
}

// ================================================================================================
// Pretty print for SignatureDataLocation

impl fmt::Display for SignatureDataLocation {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        use SignatureDataLocation::*;
        match self {
            // CallData => f.write_str("calldata"),
            Memory => f.write_str("memory"),
        }
    }
}

// ================================================================================================
// Pretty print for SolidityPrimitiveType

impl fmt::Display for SolidityPrimitiveType {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        use SolidityPrimitiveType::*;
        match self {
            Bool => f.write_str("bool"),
            Uint(n) => write!(f, "uint{}", n),
            Int(n) => write!(f, "int{}", n),
            Fixed(m, n) => write!(f, "fixed{}x{}", m, n),
            Ufixed(m, n) => write!(f, "ufixed{}x{}", m, n),
            Address(_) => f.write_str("address"),
        }
    }
}

impl SolidityPrimitiveType {
    /// Check type compatibility for primitive types
    /// TODO: int and fixed are not supported yet
    pub fn check_primitive_type_compatibility(
        &self,
        ctx: &Context,
        move_ty: &Type,
        //solidity_primitive_ty: &SolidityPrimitiveType,
    ) -> bool {
        use SolidityPrimitiveType::*;
        match self {
            Bool => move_ty.is_bool(),
            Uint(i) => self.check_uint_compatibility(ctx, *i, move_ty),
            Int(i) => self.check_uint_compatibility(ctx, *i, move_ty), // current we assume int<N> in Solidity is specified in Move as a u<M> value.
            Fixed(_, _) => false,
            Ufixed(_, _) => false,
            Address(_) => move_ty.is_signer_or_address(),
        }
    }

    /// Check whether move_ty is big enough to represent a uint number
    fn check_uint_compatibility(&self, ctx: &Context, size: usize, move_ty: &Type) -> bool {
        match move_ty {
            Type::Primitive(p) => match p {
                PrimitiveType::U8 => size == 8,
                PrimitiveType::U64 => size <= 64,
                PrimitiveType::U128 => size <= 128,
                _ => false,
            },
            Type::Struct(mid, sid, _) => ctx.is_u256(mid.qualified(*sid)),
            _ => false,
        }
    }
}

// ================================================================================================
// Pretty print for SolidityType

impl fmt::Display for SolidityType {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        use SolidityType::*;
        match self {
            Primitive(ty) => write!(f, "{}", ty),
            Tuple(tys) => {
                let s = tys
                    .iter()
                    .map(|ref t| format!("{}", t))
                    .collect::<Vec<String>>()
                    .join(",");
                write!(f, "({})", s)
            }
            DynamicArray(ty) => write!(f, "{}[]", ty),
            StaticArray(ty, n) => write!(f, "{}[{}]", ty, n),
            SolidityString => f.write_str("string"),
            Bytes => f.write_str("bytes"),
            BytesStatic(n) => write!(f, "bytes{}", n),
        }
    }
}

// ================================================================================================
// Parse solidity signatures and check type compatibility

impl SolidityType {
    /// Check whether ty is a static type in the sense of serialization
    pub fn is_static(&self) -> bool {
        use crate::solidity_ty::SolidityType::*;
        let conjunction = |tys: &[SolidityType]| {
            tys.iter()
                .map(|t| t.is_static())
                .collect::<Vec<_>>()
                .into_iter()
                .all(|t| t)
        };
        match self {
            Primitive(_) | BytesStatic(_) => true,
            Tuple(tys) => conjunction(tys),
            StaticArray(ty, _) => ty.is_static(),
            _ => false,
        }
    }

    /// Check whether a type is a value type
    fn is_value_type(&self) -> bool {
        use crate::solidity_ty::SolidityType::*;
        matches!(self, Primitive(_) | BytesStatic(_))
    }

    /// Returns the max value (bit mask) for a given type.
    pub fn max_value(&self) -> String {
        let size = self.abi_head_size(false);
        assert!(size <= 32, "unexpected type size {} for `{}`", size, self);
        let multipler = size * 8;
        format!("${{MAX_U{}}}", multipler)
    }

    /// Parse a move type into a solidity type
    fn translate_from_move(ctx: &Context, ty: &Type) -> Self {
        use PrimitiveType::*;
        use Type::*;
        let generate_tuple = |tys: &Vec<Type>| {
            let s_type = tys
                .iter()
                .map(|t| Self::translate_from_move(ctx, t))
                .collect::<Vec<_>>();
            SolidityType::Tuple(s_type)
        };
        match ty {
            Primitive(p) => match p {
                Bool => SolidityType::Primitive(SolidityPrimitiveType::Bool),
                U8 => SolidityType::Primitive(SolidityPrimitiveType::Uint(8)),
                U64 => SolidityType::Primitive(SolidityPrimitiveType::Uint(64)),
                U128 => SolidityType::Primitive(SolidityPrimitiveType::Uint(128)),
                Address => SolidityType::Primitive(SolidityPrimitiveType::Address(false)),
                Signer => SolidityType::Primitive(SolidityPrimitiveType::Address(false)),
                Num | Range | EventStore => {
                    panic!("unexpected field type")
                }
            },
            Vector(ety) => {
                SolidityType::DynamicArray(Box::new(Self::translate_from_move(ctx, ety)))
            }
            Tuple(tys) => generate_tuple(tys),
            Struct(mid, sid, _) => {
                if ctx.is_u256(mid.qualified(*sid)) {
                    SolidityType::Primitive(SolidityPrimitiveType::Uint(256))
                } else {
                    let tys = ctx.get_field_types(mid.qualified(*sid));
                    generate_tuple(&tys) // TODO: translate into tuple type?
                }
            }
            TypeParameter(_)
            | Reference(_, _)
            | Fun(_, _)
            | TypeDomain(_)
            | ResourceDomain(_, _, _)
            | Error
            | Var(_) => {
                panic!("unexpected field type")
            }
        }
    }

    /// Parse a solidity type
    /// TODO: struct is not supported yet
    fn parse(ty_str: &str) -> anyhow::Result<Self> {
        let trimmed_ty_str = ty_str.trim();
        if trimmed_ty_str.contains('[') {
            // array type
            SolidityType::parse_array(trimmed_ty_str)
        } else if check_simple_type_prefix(trimmed_ty_str) {
            // primitive and byte types
            SolidityType::parse_simple_type(trimmed_ty_str)
        } else {
            // Solidity identifier matching
            static RE_GENERAL_TYPE: Lazy<Regex> =
                Lazy::new(|| Regex::new(r"^[a-zA-Z_$][a-zA-Z_$0-9]*$").unwrap());
            let mut error_msg = "unsupported types";
            if !RE_GENERAL_TYPE.is_match(trimmed_ty_str) {
                error_msg = "illegal type name";
            }
            // TODO: struct
            Err(anyhow!(error_msg))
        }
    }

    /// Parse value, bytes and string types
    fn parse_simple_type(ty_str: &str) -> anyhow::Result<Self> {
        if ty_str == "bool" {
            return Ok(SolidityType::Primitive(SolidityPrimitiveType::Bool));
        }
        if ty_str.starts_with("uint") {
            let prefix_len = "uint".len();
            if ty_str.len() > prefix_len {
                let num = ty_str[prefix_len..]
                    .parse::<usize>()
                    .context(PARSE_ERR_MSG)?;
                if check_type_int_range(num) {
                    return Ok(SolidityType::Primitive(SolidityPrimitiveType::Uint(num)));
                }
            } else {
                return Ok(SolidityType::Primitive(SolidityPrimitiveType::Uint(256)));
            }
        }
        if ty_str.starts_with("int") {
            let prefix_len = "int".len();
            if ty_str.len() > prefix_len {
                let num = ty_str[prefix_len..]
                    .parse::<usize>()
                    .context(PARSE_ERR_MSG)?;
                if check_type_int_range(num) {
                    return Ok(SolidityType::Primitive(SolidityPrimitiveType::Int(num)));
                }
            } else {
                return Ok(SolidityType::Primitive(SolidityPrimitiveType::Int(256)));
            }
        }
        if ty_str.starts_with("address") {
            let prefix_len = "address".len();
            if ty_str.len() > prefix_len {
                let address_type_array = ty_str.split_whitespace().collect_vec();
                if address_type_array.len() == 2 && address_type_array[1] == "payable" {
                    return Ok(SolidityType::Primitive(SolidityPrimitiveType::Address(
                        true,
                    )));
                }
            } else if ty_str == "address" {
                return Ok(SolidityType::Primitive(SolidityPrimitiveType::Address(
                    false,
                )));
            }
        }
        if ty_str.starts_with("fixed") {
            let prefix_len = "fixed".len();
            if ty_str.len() > prefix_len {
                let num_str = &ty_str[prefix_len..];
                let x_pos = num_str.rfind('x').context(PARSE_ERR_MSG)?;
                let num_m = num_str[0..x_pos].parse::<usize>().context(PARSE_ERR_MSG)?;
                let num_n = num_str[x_pos + 1..]
                    .parse::<usize>()
                    .context(PARSE_ERR_MSG)?;
                if check_type_int_range(num_m) && check_fixed_n_range(num_n) {
                    return Ok(SolidityType::Primitive(SolidityPrimitiveType::Fixed(
                        num_m, num_n,
                    )));
                }
            } else {
                return Ok(SolidityType::Primitive(SolidityPrimitiveType::Fixed(
                    128, 18,
                )));
            }
        }
        if ty_str.starts_with("ufixed") {
            let prefix_len = "ufixed".len();
            if ty_str.len() > prefix_len {
                let num_str = &ty_str[prefix_len..];
                let x_pos = num_str.rfind('x').context(PARSE_ERR_MSG)?;
                let num_m = num_str[0..x_pos].parse::<usize>().context(PARSE_ERR_MSG)?;
                let num_n = num_str[x_pos + 1..]
                    .parse::<usize>()
                    .context(PARSE_ERR_MSG)?;
                if check_type_int_range(num_m) && check_fixed_n_range(num_n) {
                    return Ok(SolidityType::Primitive(SolidityPrimitiveType::Ufixed(
                        num_m, num_n,
                    )));
                }
            } else {
                return Ok(SolidityType::Primitive(SolidityPrimitiveType::Ufixed(
                    128, 18,
                )));
            }
        }
        if ty_str.starts_with("bytes") {
            let prefix_len = "bytes".len();
            if ty_str.len() > prefix_len {
                let num = ty_str[prefix_len..]
                    .parse::<usize>()
                    .context(PARSE_ERR_MSG)?;
                if check_static_bytes_range(num) {
                    return Ok(SolidityType::BytesStatic(num));
                }
            } else {
                return Ok(SolidityType::Bytes);
            }
        }
        if ty_str == "string" {
            return Ok(SolidityType::SolidityString);
        }
        Err(anyhow!(PARSE_ERR_MSG_SIMPLE_TYPE))
    }

    /// Parse array types
    fn parse_array(ty_str: &str) -> anyhow::Result<Self> {
        let last_pos = ty_str.rfind('[').context(PARSE_ERR_MSG)?;
        let out_type = SolidityType::parse(&ty_str[..last_pos])?;
        let last_indice_str = &ty_str[last_pos..].trim();
        if last_indice_str.len() >= 2
            && last_indice_str.starts_with('[')
            && last_indice_str.ends_with(']')
        {
            let length_opt = last_indice_str[1..last_indice_str.len() - 1].trim();
            if !length_opt.is_empty() {
                return Ok(SolidityType::StaticArray(
                    Box::new(out_type),
                    length_opt.parse::<usize>().context(PARSE_ERR_MSG)?,
                ));
            } else {
                return Ok(SolidityType::DynamicArray(Box::new(out_type)));
            }
        }
        Err(anyhow!(PARSE_ERR_MSG_ARRAY_TYPE))
    }

    /// Compute the data size of ty on the stack
    pub fn abi_head_size(&self, padded: bool) -> usize {
        use crate::solidity_ty::{SolidityPrimitiveType::*, SolidityType::*};
        if self.is_static() {
            match self {
                Primitive(p) => match p {
                    Bool => {
                        if padded {
                            32
                        } else {
                            1
                        }
                    }
                    Int(size) | Uint(size) | Fixed(size, _) | Ufixed(size, _) => {
                        if padded {
                            32
                        } else {
                            size / 8
                        }
                    }
                    Address(_) => {
                        if padded {
                            32
                        } else {
                            20
                        }
                    }
                },
                StaticArray(ty, size) => {
                    let mut size = ty.abi_head_size(padded) * size;
                    if padded {
                        size = ((size + 31) / 32) * 32;
                    }
                    size
                }
                BytesStatic(size) => {
                    if padded {
                        32
                    } else {
                        size * 8
                    }
                }
                Tuple(tys) => abi_head_sizes_sum(tys, padded),
                _ => panic!("unexpected field type"),
            }
        } else {
            // Dynamic types
            32
        }
    }

    /// Check whether a solidity type is compatible with its corresponding move type
    /// TODO: int<M>, fixed, struct are not supported yets
    fn check_type_compatibility(&self, ctx: &Context, move_ty: &Type) -> bool {
        match self {
            SolidityType::Primitive(p) => p.check_primitive_type_compatibility(ctx, move_ty),
            SolidityType::DynamicArray(array_type) | SolidityType::StaticArray(array_type, _) => {
                if let Type::Vector(ety) = move_ty {
                    array_type.check_type_compatibility(ctx, ety)
                } else {
                    false
                }
            }
            SolidityType::SolidityString => {
                if let Type::Struct(mid, sid, _) = move_ty {
                    ctx.is_string(mid.qualified(*sid))
                } else if let Type::Vector(ety) = move_ty {
                    matches!(**ety, Type::Primitive(PrimitiveType::U8))
                } else {
                    false
                }
            }
            SolidityType::Bytes | SolidityType::BytesStatic(_) => {
                if let Type::Vector(ety) = move_ty {
                    matches!(**ety, Type::Primitive(PrimitiveType::U8))
                } else {
                    false
                }
            }
            SolidityType::Tuple(_) => panic!("unexpected solidity type"),
        }
    }
}

// ================================================================================================
// Pretty print for SoliditySignature

impl fmt::Display for SoliditySignature {
    fn fmt(&self, f: &mut Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.selector_signature())
    }
}

impl SoliditySignature {
    /// Create a default solidity signature from a move function signature
    pub fn create_default_solidity_signature(ctx: &Context, fun: &FunctionEnv<'_>) -> Self {
        let fun_name = fun.symbol_pool().string(fun.get_name()).to_string();
        let mut para_type_lst = vec![];
        for move_ty in fun.get_parameter_types() {
            let solidity_ty = SolidityType::translate_from_move(ctx, &move_ty); // implicit mapping from a move type to a solidity type
            para_type_lst.push((solidity_ty, SignatureDataLocation::Memory)); // memory is used by default
        }
        let mut ret_type_lst = vec![];
        for move_ty in fun.get_return_types() {
            let solidity_ty = SolidityType::translate_from_move(ctx, &move_ty);
            ret_type_lst.push((solidity_ty, SignatureDataLocation::Memory));
        }
        SoliditySignature {
            sig_name: fun_name,
            para_types: para_type_lst,
            ret_types: ret_type_lst,
        }
    }

    /// Generate parameter list for computing the function selector
    fn compute_param_types(&self, param_types: &[&SolidityType]) -> String {
        let display_type_slice = |tys: &[&SolidityType]| -> String {
            tys.iter()
                .map(|t| format!("{}", t))
                .collect::<Vec<_>>()
                .join(",")
        };
        display_type_slice(param_types)
    }

    fn selector_signature(&self) -> String {
        format!(
            "{}({})",
            self.sig_name,
            self.compute_param_types(&self.para_types.iter().map(|(ty, _)| ty).collect_vec())
        )
    }

    /// Parse the solidity signature
    pub fn parse_into_solidity_signature(sig_str: &str) -> anyhow::Result<Self> {
        // Solidity signature matching
        static SIG_REG: Lazy<Regex> = Lazy::new(|| {
            Regex::new(
                r"^\s*(?P<sig_name>[a-zA-Z_$][a-zA-Z_$0-9]*)\s*\((?P<args>[^)]*)\)(?P<ret_ty>.*)?",
            )
            .unwrap()
        });
        if let Some(parsed) = SIG_REG.captures(sig_str.trim()) {
            let sig_name = parsed.name("sig_name").context(PARSE_ERR_MSG)?.as_str();
            let para_type_str = parsed.name("args").context(PARSE_ERR_MSG)?.as_str();
            let ret_ty_str_opt = parsed.name("ret_ty");
            let mut ret_ty = "";
            if let Some(ret_ty_str) = ret_ty_str_opt {
                let ret_ty_str_trim = ret_ty_str.as_str().trim();
                if !ret_ty_str_trim.is_empty() {
                    let mut parse_error = false;
                    if let Some(stripped_returns) = ret_ty_str_trim.strip_prefix("returns") {
                        let stripped_returns_trim = stripped_returns.trim();
                        if stripped_returns_trim.starts_with('(')
                            && stripped_returns_trim.ends_with(')')
                        {
                            ret_ty = &stripped_returns_trim[1..stripped_returns_trim.len() - 1];
                        } else {
                            parse_error = true;
                        }
                    } else {
                        parse_error = true;
                    }
                    if parse_error {
                        return Err(anyhow!(PARSE_ERR_MSG_RETURN));
                    }
                }
            }
            let solidity_sig = SoliditySignature {
                sig_name: sig_name.to_string(),
                para_types: SoliditySignature::extract_para_type_str(para_type_str)?,
                ret_types: SoliditySignature::extract_para_type_str(ret_ty)?,
            };
            Ok(solidity_sig)
        } else {
            Err(anyhow!(PARSE_ERR_MSG))
        }
    }

    /// Generate pairs of solidity type and location
    fn extract_para_type_str(
        args: &str,
    ) -> anyhow::Result<Vec<(SolidityType, SignatureDataLocation)>> {
        let args_trim = args.trim();
        if args_trim.is_empty() {
            return Ok(vec![]);
        }
        let mut ret_vec = vec![];
        let paras = args_trim.split(',').collect_vec();
        for para in paras {
            let para_trim = para.trim();
            if para_trim.is_empty() {
                return Err(anyhow!(PARSE_ERR_MSG));
            }
            let mut data_location = SignatureDataLocation::Memory;
            let mut para_type_str = para_trim;
            let mut loc_flag = false;
            if let Some(stripped_memory) = para_trim.strip_suffix("memory") {
                data_location = SignatureDataLocation::Memory;
                para_type_str = stripped_memory;
                loc_flag = true;
            } else if let Some(_stripped_calldata) = para_trim.strip_suffix("calldata") {
                return Err(anyhow!("calldata is not supported yet"));
            }
            let ty = SolidityType::parse(para_type_str)?;
            if loc_flag && ty.is_value_type() {
                return Err(anyhow!(
                    "data location can only be specified for array or struct types"
                ));
            }
            ret_vec.push((ty, data_location));
        }
        Ok(ret_vec)
    }

    /// Check whether the user defined solidity signature is compatible with the Move signature
    pub fn check_sig_compatibility(&self, ctx: &Context, fun: &FunctionEnv<'_>) -> bool {
        let para_types = fun.get_parameter_types();
        let sig_para_vec = self.para_types.iter().map(|(ty, _)| ty).collect::<Vec<_>>();
        if para_types.len() != sig_para_vec.len() {
            return false;
        }
        // Check parameter type list
        for type_pair in para_types.iter().zip(sig_para_vec.iter()) {
            let (m_ty, s_ty) = type_pair;
            if !s_ty.check_type_compatibility(ctx, m_ty) {
                return false;
            }
        }
        // Check return type list
        let sig_ret_vec = self.ret_types.iter().map(|(ty, _)| ty).collect::<Vec<_>>();
        let ret_types = fun.get_return_types();
        if ret_types.len() != sig_ret_vec.len() {
            return false;
        }
        for type_pair in ret_types.iter().zip(sig_ret_vec.iter()) {
            let (m_ty, s_ty) = type_pair;
            if !s_ty.check_type_compatibility(ctx, m_ty) {
                return false;
            }
        }
        true
    }
}

fn check_simple_type_prefix(ty_str: &str) -> bool {
    /// Prefixes of value, bytes and string related types
    const SIMPLE_TYPE_PREFIX: &[&str] = &[
        "uint", "int", "ufixed", "fixed", "bool", "address", "bytes", "string",
    ];
    for prefix in SIMPLE_TYPE_PREFIX {
        if ty_str.starts_with(prefix) {
            return true;
        }
    }
    false
}

fn check_type_int_range(num: usize) -> bool {
    (8..=256).contains(&num) && num % 8 == 0
}

fn check_fixed_n_range(num: usize) -> bool {
    num <= 80
}

fn check_static_bytes_range(num: usize) -> bool {
    (1..=32).contains(&num)
}

/// Mangle a slice of solidity types.
pub(crate) fn mangle_solidity_types(tys: &[SolidityType]) -> String {
    if tys.is_empty() {
        "".to_owned()
    } else {
        format!("${}$", tys.iter().join("_"))
    }
}

/// Compute the sum of data size of tys
pub(crate) fn abi_head_sizes_sum(tys: &[SolidityType], padded: bool) -> usize {
    let size_vec = abi_head_sizes_vec(tys, padded);
    size_vec.iter().map(|(_, size)| size).sum()
}

/// Compute the data size of all types in tys
pub(crate) fn abi_head_sizes_vec(tys: &[SolidityType], padded: bool) -> Vec<(SolidityType, usize)> {
    tys.iter()
        .map(|ty_| (ty_.clone(), ty_.abi_head_size(padded)))
        .collect_vec()
}
