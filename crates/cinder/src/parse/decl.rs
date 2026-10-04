//! Declarations: specifiers, declarators, struct/union/enum, initializers.

use super::*;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum SpecCtx {
    External,
    Block,
    Member,
    Param,
    TypeName,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Abstractness {
    /// A name is mandatory (ordinary declarations).
    Never,
    /// A name is optional (parameters).
    Allowed,
    /// No name allowed (type names).
    Required,
}

#[derive(Default, Clone, Copy)]
struct BaseCounts {
    void: u8,
    ch: u8,
    short: u8,
    int: u8,
    long: u8,
    float: u8,
    double: u8,
    signed: u8,
    unsigned: u8,
    bool_: u8,
    complex: u8,
}

impl BaseCounts {
    fn total(&self) -> u8 {
        self.void
            + self.ch
            + self.short
            + self.int
            + self.long
            + self.float
            + self.double
            + self.signed
            + self.unsigned
            + self.bool_
    }

    fn any(&self) -> bool {
        self.total() + self.complex > 0
    }

    /// Is this (partial) combination of specifier keywords still valid?
    fn valid(&self) -> bool {
        let t = self.total();
        if self.void > 0 || self.bool_ > 0 {
            return t == 1 && (self.void + self.bool_) == 1;
        }
        if self.float > 0 {
            return t == 1 && self.float == 1;
        }
        if self.double > 0 {
            return self.double == 1
                && self.long <= 1
                && self.ch + self.short + self.int + self.signed + self.unsigned == 0;
        }
        if self.ch > 0 {
            return self.ch == 1 && self.short + self.int + self.long == 0 && self.signed + self.unsigned <= 1;
        }
        self.short <= 1
            && self.long <= 2
            && !(self.short > 0 && self.long > 0)
            && self.int <= 1
            && self.signed <= 1
            && self.unsigned <= 1
            && !(self.signed > 0 && self.unsigned > 0)
    }

    fn resolve(&self) -> BaseType {
        use BaseType::*;
        if self.void > 0 {
            return Void;
        }
        if self.bool_ > 0 {
            return Bool;
        }
        if self.float > 0 {
            return Float;
        }
        if self.double > 0 {
            return if self.long > 0 { LongDouble } else { Double };
        }
        let u = self.unsigned > 0;
        if self.ch > 0 {
            return if u {
                UChar
            } else if self.signed > 0 {
                SChar
            } else {
                Char
            };
        }
        if self.short > 0 {
            return if u { UShort } else { Short };
        }
        match self.long {
            1 => {
                if u {
                    ULong
                } else {
                    Long
                }
            }
            2 => {
                if u {
                    ULongLong
                } else {
                    LongLong
                }
            }
            _ => {
                if u {
                    UInt
                } else {
                    Int
                }
            }
        }
    }
}

fn base_kw_name(k: Kw) -> &'static str {
    k.spelling()
}

impl<'a> Parser<'a> {
    // ───────────────────────────── predicates ─────────────────────────────

    /// Does the current token begin a type name (as in a cast or `sizeof(`)?
    pub(crate) fn is_type_start(&self) -> bool {
        self.type_start_at(0)
    }

    pub(crate) fn type_start_at(&self, n: usize) -> bool {
        match self.peek_n(n).kind {
            PKind::Kw(k) => matches!(
                k,
                Kw::Void
                    | Kw::Char
                    | Kw::Short
                    | Kw::Int
                    | Kw::Long
                    | Kw::Float
                    | Kw::Double
                    | Kw::Signed
                    | Kw::Unsigned
                    | Kw::Bool
                    | Kw::Complex
                    | Kw::Struct
                    | Kw::Union
                    | Kw::Enum
                    | Kw::Const
                    | Kw::Volatile
                    | Kw::Restrict
                    | Kw::Atomic
                    | Kw::Typeof
                    | Kw::BuiltinVaList
                    | Kw::Attribute
                    | Kw::Extension
            ),
            PKind::Ident(name) => self.is_typedef_name(name),
            _ => false,
        }
    }

    /// Does the current token begin a declaration (block-scope)?
    pub(crate) fn is_declaration_start(&self) -> bool {
        match self.kind() {
            PKind::Kw(k) => {
                self.is_type_start()
                    || matches!(
                        k,
                        Kw::Typedef
                            | Kw::Extern
                            | Kw::Static
                            | Kw::Auto
                            | Kw::Register
                            | Kw::Inline
                            | Kw::Noreturn
                            | Kw::ThreadLocal
                            | Kw::Alignas
                    )
            }
            PKind::Ident(name) => {
                if self.peek_n(1).kind == PKind::Punct(Punct::Colon) {
                    return false; // a label
                }
                if self.is_typedef_name(name) {
                    return true;
                }
                // `foo bar;` with an undeclared `foo`: a declaration with an unknown type.
                matches!(self.peek_n(1).kind, PKind::Ident(_))
            }
            _ => false,
        }
    }

    // ───────────────────────────── attributes ─────────────────────────────

    pub(crate) fn parse_attrs(&mut self) -> Vec<Attr> {
        let mut out = Vec::new();
        while self.at_kw(Kw::Attribute) {
            let kw = self.bump();
            if !self.eat_punct(Punct::LParen) || !self.eat_punct(Punct::LParen) {
                let _ = self.error_at(kw.span, "expected '((' after '__attribute__'");
                return out;
            }
            loop {
                if self.at_punct(Punct::RParen) {
                    break;
                }
                let name = match self.kind() {
                    PKind::Ident(n) => Some(n),
                    PKind::Kw(k) => Some(Symbol::new(k.spelling())),
                    _ => None,
                };
                let Some(n) = name else {
                    let _ = self.error_here("expected attribute name");
                    break;
                };
                let t = self.bump();
                // Normalize `__packed__` -> `packed`.
                let s = n.as_str();
                let n = if s.starts_with("__") && s.ends_with("__") && s.len() > 4 {
                    Symbol::new(&s[2..s.len() - 2])
                } else {
                    n
                };
                let mut args = Vec::new();
                if self.eat_punct(Punct::LParen) {
                    let open = self.prev_span();
                    while !self.at_punct(Punct::RParen) && !self.at_eof() {
                        match self.parse_assign() {
                            Ok(e) => args.push(e),
                            Err(()) => {
                                // skip to the closing paren of this attribute
                                while !self.at_punct(Punct::RParen) && !self.at_eof() {
                                    self.bump();
                                }
                                break;
                            }
                        }
                        if !self.eat_punct(Punct::Comma) {
                            break;
                        }
                    }
                    let _ = self.expect_close(Punct::RParen, open);
                }
                out.push(Attr { name: Ident { name: n, span: t.span }, args });
                if !self.eat_punct(Punct::Comma) {
                    break;
                }
            }
            let open = kw.span;
            let _ = self.expect_close(Punct::RParen, open);
            let _ = self.expect_close(Punct::RParen, open);
        }
        out
    }

    // ───────────────────────────── specifiers ─────────────────────────────

    pub(crate) fn parse_decl_specs(&mut self, ctx: SpecCtx) -> PResult<DeclSpecs> {
        let start = self.span();
        let mut specs = DeclSpecs { span: start, ..Default::default() };
        let mut counts = BaseCounts::default();
        let mut base_span: Option<Span> = None;
        let mut explicit: Option<TypeSpec> = None;

        loop {
            let tok = self.peek();
            // `struct S { ... } int x;`: a new type specifier after a tag
            // definition means the `;` was forgotten; let the caller say so.
            let starts_new_type = matches!(
                tok.kind,
                PKind::Kw(
                    Kw::Struct
                        | Kw::Union
                        | Kw::Enum
                        | Kw::Void
                        | Kw::Char
                        | Kw::Short
                        | Kw::Int
                        | Kw::Long
                        | Kw::Float
                        | Kw::Double
                        | Kw::Signed
                        | Kw::Unsigned
                        | Kw::Bool
                )
            );
            if starts_new_type && explicit.as_ref().is_some_and(|t| tag_keyword(&t.kind).is_some()) {
                break;
            }
            match tok.kind {
                PKind::Kw(Kw::Typedef | Kw::Extern | Kw::Static | Kw::Auto | Kw::Register) => {
                    let PKind::Kw(k) = tok.kind else { unreachable!() };
                    self.bump();
                    let sc = match k {
                        Kw::Typedef => StorageClass::Typedef,
                        Kw::Extern => StorageClass::Extern,
                        Kw::Static => StorageClass::Static,
                        Kw::Auto => StorageClass::Auto,
                        _ => StorageClass::Register,
                    };
                    if matches!(ctx, SpecCtx::Member | SpecCtx::TypeName) {
                        let what = if ctx == SpecCtx::Member { "struct or union member" } else { "type name" };
                        let _ = self.error_at(tok.span, format!("'{}' is not allowed on a {}", k.spelling(), what));
                    } else if let Some((prev, _)) = specs.storage {
                        let msg =
                            format!("cannot combine with previous '{}' declaration specifier", storage_name(prev));
                        let _ = self.error_at(tok.span, msg);
                    } else {
                        specs.storage = Some((sc, tok.span));
                    }
                }
                PKind::Kw(Kw::ThreadLocal) => {
                    self.bump();
                    specs.thread_local = true;
                }
                PKind::Kw(Kw::Inline) => {
                    self.bump();
                    specs.inline = true;
                }
                PKind::Kw(Kw::Noreturn) => {
                    self.bump();
                    specs.noreturn = true;
                }
                PKind::Kw(Kw::Const) => {
                    self.bump();
                    specs.quals.is_const = true;
                }
                PKind::Kw(Kw::Volatile) => {
                    self.bump();
                    specs.quals.is_volatile = true;
                }
                PKind::Kw(Kw::Restrict) => {
                    self.bump();
                    specs.quals.is_restrict = true;
                }
                PKind::Kw(Kw::Extension) => {
                    self.bump();
                }
                PKind::Kw(Kw::Attribute) => {
                    let mut a = self.parse_attrs();
                    specs.attrs.append(&mut a);
                }
                PKind::Kw(Kw::Atomic) => {
                    self.bump();
                    if self.at_punct(Punct::LParen) && explicit.is_none() && !counts.any() {
                        let open = self.span();
                        self.bump();
                        let ty = self.parse_type_name()?;
                        self.expect_close(Punct::RParen, open)?;
                        explicit =
                            Some(TypeSpec { kind: TypeSpecKind::Atomic(Box::new(ty)), span: self.span_from(tok.span) });
                    } else {
                        specs.quals.is_atomic = true;
                    }
                }
                PKind::Kw(Kw::Alignas) => {
                    self.bump();
                    let open = self.span();
                    if !self.eat_punct(Punct::LParen) {
                        self.error_here("expected '(' after '_Alignas'")?;
                    }
                    let spec = if self.is_type_start() {
                        AlignSpec::Type(Box::new(self.parse_type_name()?))
                    } else {
                        AlignSpec::Expr(Box::new(self.parse_cond()?))
                    };
                    self.expect_close(Punct::RParen, open)?;
                    specs.align = Some((spec, self.span_from(tok.span)));
                }
                PKind::Kw(
                    k @ (Kw::Void
                    | Kw::Char
                    | Kw::Short
                    | Kw::Int
                    | Kw::Long
                    | Kw::Float
                    | Kw::Double
                    | Kw::Signed
                    | Kw::Unsigned
                    | Kw::Bool
                    | Kw::Complex),
                ) => {
                    self.bump();
                    if explicit.is_some() {
                        let _ = self.error_at(
                            tok.span,
                            format!("cannot combine '{}' with a previous type specifier", base_kw_name(k)),
                        );
                        continue;
                    }
                    if k == Kw::Complex {
                        let _ = self.error_at(tok.span, "not yet supported: _Complex types");
                        continue;
                    }
                    let mut c = counts;
                    match k {
                        Kw::Void => c.void += 1,
                        Kw::Char => c.ch += 1,
                        Kw::Short => c.short += 1,
                        Kw::Int => c.int += 1,
                        Kw::Long => c.long += 1,
                        Kw::Float => c.float += 1,
                        Kw::Double => c.double += 1,
                        Kw::Signed => c.signed += 1,
                        Kw::Unsigned => c.unsigned += 1,
                        _ => c.bool_ += 1,
                    }
                    if c.valid() {
                        counts = c;
                    } else {
                        let _ = self.error_at(
                            tok.span,
                            format!("cannot combine '{}' with the previous type specifiers", base_kw_name(k)),
                        );
                    }
                    base_span = Some(base_span.map_or(tok.span, |s| s.to(tok.span)));
                }
                PKind::Kw(k @ (Kw::Struct | Kw::Union)) => {
                    if explicit.is_some() || counts.any() {
                        let _ = self.error_at(
                            tok.span,
                            format!("cannot combine '{}' with a previous type specifier", k.spelling()),
                        );
                    }
                    let rec = self.parse_record_spec(k == Kw::Union)?;
                    let span = rec.span;
                    explicit = Some(TypeSpec { kind: TypeSpecKind::Record(rec), span });
                }
                PKind::Kw(Kw::Enum) => {
                    if explicit.is_some() || counts.any() {
                        let _ = self.error_at(tok.span, "cannot combine 'enum' with a previous type specifier");
                    }
                    let en = self.parse_enum_spec()?;
                    let span = en.span;
                    explicit = Some(TypeSpec { kind: TypeSpecKind::Enum(en), span });
                }
                PKind::Kw(Kw::BuiltinVaList) => {
                    self.bump();
                    explicit = Some(TypeSpec { kind: TypeSpecKind::Base(BaseType::VaList), span: tok.span });
                }
                PKind::Kw(Kw::Typeof) => {
                    self.error_here("not yet supported: typeof")?;
                }
                PKind::Ident(name) if explicit.is_none() && !counts.any() => {
                    if self.is_typedef_name(name) {
                        self.bump();
                        explicit = Some(TypeSpec {
                            kind: TypeSpecKind::Typedef(Ident { name, span: tok.span }),
                            span: tok.span,
                        });
                    } else if self.unknown_type_follows() {
                        self.bump();
                        let _ = self.error_at(tok.span, format!("unknown type name '{}'", name));
                        // Recover as `int` so the rest of the declaration still parses.
                        explicit = Some(TypeSpec { kind: TypeSpecKind::Base(BaseType::Int), span: tok.span });
                    } else {
                        break;
                    }
                }
                _ => break,
            }
        }

        specs.span = self.span_from(start);
        specs.ty = match explicit {
            Some(t) => Some(t),
            None if counts.any() => {
                Some(TypeSpec { kind: TypeSpecKind::Base(counts.resolve()), span: base_span.unwrap_or(start) })
            }
            None => None,
        };
        Ok(specs)
    }

    /// After `struct S { ... }` (a tag *definition*) the next token must
    /// start a declarator or be `;`. If it cannot, the `;` was forgotten.
    fn missing_semi_after_tag(&self, specs: &DeclSpecs) -> Option<&'static str> {
        let kw = tag_keyword(&specs.ty.as_ref()?.kind)?;
        let declarator_next = matches!(
            self.kind(),
            PKind::Punct(Punct::Star | Punct::LParen | Punct::Semi) | PKind::Ident(_) | PKind::Kw(Kw::Attribute)
        );
        if declarator_next {
            None
        } else {
            Some(kw)
        }
    }

    /// After an unrecognised identifier in specifier position: does what
    /// follows look like `<type> <declarator>`?
    fn unknown_type_follows(&self) -> bool {
        matches!(
            self.peek_n(1).kind,
            PKind::Ident(_) | PKind::Punct(Punct::Star) | PKind::Kw(Kw::Const | Kw::Volatile | Kw::Restrict)
        )
    }

    // ───────────────────────────── struct / union / enum ─────────────────────────────

    fn parse_record_spec(&mut self, is_union: bool) -> PResult<RecordSpec> {
        let kw = self.bump();
        let pack = self.cur_pack;
        let mut attrs = self.parse_attrs();
        let tag = match self.kind() {
            PKind::Ident(name) => {
                let t = self.bump();
                Some(Ident { name, span: t.span })
            }
            _ => None,
        };
        let mut attrs2 = self.parse_attrs();
        attrs.append(&mut attrs2);
        let mut members = None;
        if self.at_punct(Punct::LBrace) {
            let open = self.bump().span;
            let mut list = Vec::new();
            while !self.at_punct(Punct::RBrace) && !self.at_eof() {
                match self.parse_member_decl() {
                    Ok(Some(m)) => list.push(m),
                    Ok(None) => {}
                    Err(()) => self.sync_member(),
                }
            }
            self.expect_close(Punct::RBrace, open)?;
            members = Some(list);
        } else if tag.is_none() {
            self.error_here(format!(
                "expected identifier or '{{' after '{}'",
                if is_union { "union" } else { "struct" }
            ))?;
        }
        let mut attrs3 = self.parse_attrs();
        attrs.append(&mut attrs3);
        Ok(RecordSpec { is_union, tag, members, pack, attrs, span: self.span_from(kw.span) })
    }

    /// Recover inside a struct body: skip to the next `;` (consumed) or `}`.
    fn sync_member(&mut self) {
        let mut depth = 0i32;
        loop {
            match self.kind() {
                PKind::Eof => return,
                PKind::Punct(Punct::Semi) if depth == 0 => {
                    self.bump();
                    return;
                }
                PKind::Punct(Punct::RBrace) if depth == 0 => return,
                PKind::Punct(Punct::LBrace) => {
                    depth += 1;
                    self.bump();
                }
                PKind::Punct(Punct::RBrace) => {
                    depth -= 1;
                    self.bump();
                }
                _ => {
                    self.bump();
                }
            }
        }
    }

    fn parse_member_decl(&mut self) -> PResult<Option<MemberDecl>> {
        if self.at_kw(Kw::StaticAssert) {
            return Ok(Some(MemberDecl::StaticAssert(self.parse_static_assert()?)));
        }
        if self.eat_punct(Punct::Semi) {
            return Ok(None);
        }
        let start = self.span();
        let specs = self.parse_decl_specs(SpecCtx::Member)?;
        if specs.ty.is_none() {
            self.error_here("expected specifier-qualifier-list or '}'")?;
        }
        let mut declarators = Vec::new();
        if !self.at_punct(Punct::Semi) {
            loop {
                let mut md = MemberDeclarator { declarator: None, bit_width: None };
                if !self.at_punct(Punct::Colon) {
                    md.declarator = Some(self.parse_declarator(Abstractness::Never)?);
                }
                if self.eat_punct(Punct::Colon) {
                    md.bit_width = Some(self.parse_cond()?);
                }
                let mut tail = self.parse_attrs();
                if let Some(d) = md.declarator.as_mut() {
                    d.attrs.append(&mut tail);
                }
                declarators.push(md);
                if !self.eat_punct(Punct::Comma) {
                    break;
                }
            }
        }
        self.expect_semi("at end of declaration list");
        Ok(Some(MemberDecl::Field { specs, declarators, span: self.span_from(start) }))
    }

    fn parse_enum_spec(&mut self) -> PResult<EnumSpec> {
        let kw = self.bump();
        let _ = self.parse_attrs();
        let tag = match self.kind() {
            PKind::Ident(name) => {
                let t = self.bump();
                Some(Ident { name, span: t.span })
            }
            _ => None,
        };
        let mut enumerators = None;
        if self.at_punct(Punct::LBrace) {
            let open = self.bump().span;
            let mut list = Vec::new();
            while !self.at_punct(Punct::RBrace) && !self.at_eof() {
                let name = match self.expect_ident("identifier") {
                    Ok(n) => n,
                    Err(()) => {
                        // skip to the next comma or the closing brace
                        while !matches!(self.kind(), PKind::Punct(Punct::Comma | Punct::RBrace) | PKind::Eof) {
                            self.bump();
                        }
                        if self.eat_punct(Punct::Comma) {
                            continue;
                        }
                        break;
                    }
                };
                let _ = self.parse_attrs();
                let value = if self.eat_punct(Punct::Eq) { Some(self.parse_cond()?) } else { None };
                // Enumerators are ordinary identifiers: they hide typedef names.
                self.declare(name.name, false);
                list.push(Enumerator { name, value });
                if !self.eat_punct(Punct::Comma) {
                    break;
                }
            }
            self.expect_close(Punct::RBrace, open)?;
            enumerators = Some(list);
        } else if tag.is_none() {
            self.error_here("expected identifier or '{' after 'enum'")?;
        }
        Ok(EnumSpec { tag, enumerators, span: self.span_from(kw.span) })
    }

    // ───────────────────────────── declarators ─────────────────────────────

    pub(crate) fn parse_declarator(&mut self, abs: Abstractness) -> PResult<Declarator> {
        let start = self.span();
        let mut pointers = Vec::new();
        let mut attrs = Vec::new();
        while self.at_punct(Punct::Star) {
            let star = self.bump();
            let mut quals = Quals::default();
            loop {
                match self.kind() {
                    PKind::Kw(Kw::Const) => quals.is_const = true,
                    PKind::Kw(Kw::Volatile) => quals.is_volatile = true,
                    PKind::Kw(Kw::Restrict) => quals.is_restrict = true,
                    PKind::Kw(Kw::Atomic) => quals.is_atomic = true,
                    PKind::Kw(Kw::Attribute) => {
                        let mut a = self.parse_attrs();
                        attrs.append(&mut a);
                        continue;
                    }
                    _ => break,
                }
                self.bump();
            }
            pointers.push(PointerQuals { quals, span: self.span_from(star.span) });
        }

        let inner = match self.kind() {
            PKind::Ident(name) if abs != Abstractness::Required => {
                let t = self.bump();
                DeclaratorCore::Name(Ident { name, span: t.span })
            }
            PKind::Punct(Punct::LParen) => {
                // Nested declarator, or (for abstract declarators) the start of a parameter list.
                let n1 = self.peek_n(1).kind;
                let nested = match abs {
                    Abstractness::Never => true,
                    _ => match n1 {
                        PKind::Punct(Punct::Star | Punct::LParen | Punct::LBracket) => true,
                        PKind::Ident(n) => !self.is_typedef_name(n),
                        PKind::Kw(Kw::Attribute) => true,
                        _ => false,
                    },
                };
                if nested {
                    let open = self.bump().span;
                    let d = self.parse_declarator(abs)?;
                    self.expect_close(Punct::RParen, open)?;
                    DeclaratorCore::Nested(Box::new(d))
                } else {
                    DeclaratorCore::Abstract
                }
            }
            _ if abs == Abstractness::Never => {
                self.error_here("expected identifier or '('")?;
                unreachable!()
            }
            _ => DeclaratorCore::Abstract,
        };

        let mut suffixes = Vec::new();
        loop {
            match self.kind() {
                PKind::Punct(Punct::LBracket) => suffixes.push(self.parse_array_suffix()?),
                PKind::Punct(Punct::LParen) => suffixes.push(self.parse_function_suffix()?),
                _ => break,
            }
        }
        let mut tail = self.parse_attrs();
        attrs.append(&mut tail);
        if self.at_kw(Kw::Asm) {
            self.error_here("not yet supported: assembler labels on declarations")?;
        }
        Ok(Declarator { span: self.span_from(start), pointers, inner, suffixes, attrs })
    }

    fn parse_array_suffix(&mut self) -> PResult<Suffix> {
        let open = self.bump();
        let mut quals = Quals::default();
        let mut is_static = false;
        loop {
            match self.kind() {
                PKind::Kw(Kw::Static) => is_static = true,
                PKind::Kw(Kw::Const) => quals.is_const = true,
                PKind::Kw(Kw::Volatile) => quals.is_volatile = true,
                PKind::Kw(Kw::Restrict) => quals.is_restrict = true,
                _ => break,
            }
            self.bump();
        }
        let size = if self.at_punct(Punct::RBracket) {
            ArraySize::Unspecified
        } else if self.at_punct(Punct::Star) && self.peek_n(1).kind == PKind::Punct(Punct::RBracket) {
            self.bump();
            ArraySize::Star
        } else {
            ArraySize::Expr(Box::new(self.parse_assign()?))
        };
        self.expect_close(Punct::RBracket, open.span)?;
        Ok(Suffix::Array { size, quals, is_static, span: self.span_from(open.span) })
    }

    fn parse_function_suffix(&mut self) -> PResult<Suffix> {
        let open = self.bump();
        let mut params: Vec<ParamDecl> = Vec::new();
        let mut variadic = false;
        if !self.at_punct(Punct::RParen) {
            loop {
                if self.at_punct(Punct::Ellipsis) {
                    let e = self.bump();
                    if params.is_empty() {
                        let _ = self.error_at(e.span, "ISO C requires a named parameter before '...'");
                    }
                    variadic = true;
                    break;
                }
                let pstart = self.span();
                // K&R identifier list: `f(a, b)`
                if let PKind::Ident(n) = self.kind() {
                    if !self.is_typedef_name(n)
                        && matches!(self.peek_n(1).kind, PKind::Punct(Punct::Comma | Punct::RParen))
                    {
                        self.error_here("not yet supported: old-style (K&R) function declarators")?;
                    }
                }
                let specs = self.parse_decl_specs(SpecCtx::Param)?;
                if specs.ty.is_none() {
                    self.error_here("expected a type specifier for the parameter")?;
                }
                let declarator = self.parse_declarator(Abstractness::Allowed)?;
                params.push(ParamDecl { specs, declarator, span: self.span_from(pstart) });
                if !self.eat_punct(Punct::Comma) {
                    break;
                }
            }
        }
        self.expect_close(Punct::RParen, open.span)?;
        Ok(Suffix::Function { params, variadic, span: self.span_from(open.span) })
    }

    pub(crate) fn parse_type_name(&mut self) -> PResult<TypeName> {
        let start = self.span();
        let specs = self.parse_decl_specs(SpecCtx::TypeName)?;
        if specs.ty.is_none() {
            self.error_here("expected a type")?;
        }
        let declarator = self.parse_declarator(Abstractness::Required)?;
        Ok(TypeName { specs, declarator, span: self.span_from(start) })
    }

    // ───────────────────────────── initializers ─────────────────────────────

    pub(crate) fn parse_initializer(&mut self) -> PResult<Initializer> {
        if self.at_punct(Punct::LBrace) {
            Ok(Initializer::List(self.parse_init_list()?))
        } else {
            Ok(Initializer::Expr(self.parse_assign()?))
        }
    }

    pub(crate) fn parse_init_list(&mut self) -> PResult<InitList> {
        let open = self.bump();
        let mut items = Vec::new();
        while !self.at_punct(Punct::RBrace) && !self.at_eof() {
            let mut designators = Vec::new();
            loop {
                if self.at_punct(Punct::Dot) {
                    self.bump();
                    designators.push(Designator::Field(self.expect_ident("field designator")?));
                } else if self.at_punct(Punct::LBracket) {
                    let ob = self.bump().span;
                    let idx = self.parse_cond()?;
                    self.expect_close(Punct::RBracket, ob)?;
                    designators.push(Designator::Index(idx));
                } else {
                    break;
                }
            }
            if !designators.is_empty() && !self.eat_punct(Punct::Eq) {
                self.error_after_prev("expected '=' or another designator", "=")?;
            }
            let init = self.parse_initializer()?;
            items.push(InitItem { designators, init });
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        self.expect_close(Punct::RBrace, open.span)?;
        Ok(InitList { items, span: self.span_from(open.span) })
    }

    // ───────────────────────────── _Static_assert ─────────────────────────────

    pub(crate) fn parse_static_assert(&mut self) -> PResult<StaticAssert> {
        let kw = self.bump();
        let open = self.span();
        if !self.eat_punct(Punct::LParen) {
            self.error_here("expected '(' after '_Static_assert'")?;
        }
        let cond = self.parse_cond()?;
        let mut message = None;
        if self.eat_punct(Punct::Comma) {
            match self.kind() {
                PKind::Str(_) => {
                    let e = self.parse_primary()?;
                    if let ExprKind::StrLit { units, .. } = &e.kind {
                        message = Some(units.iter().filter_map(|&u| char::from_u32(u)).collect());
                    }
                }
                _ => self.error_here("expected string literal as the _Static_assert message")?,
            }
        }
        self.expect_close(Punct::RParen, open)?;
        self.expect_semi("after '_Static_assert'");
        Ok(StaticAssert { cond, message, span: self.span_from(kw.span) })
    }

    // ───────────────────────────── declarations ─────────────────────────────

    /// A block-scope (or `for`-init) declaration, including its `;`.
    pub(crate) fn parse_declaration(&mut self, ctx: SpecCtx) -> PResult<Declaration> {
        let start = self.span();
        let specs = self.parse_decl_specs(ctx)?;
        let mut declarators = Vec::new();
        if self.at_punct(Punct::Semi) {
            self.bump();
            return Ok(Declaration { specs, declarators, span: self.span_from(start) });
        }
        if let Some(kw) = self.missing_semi_after_tag(&specs) {
            let _ = self.error_after_prev(format!("expected ';' after {}", kw), ";");
            return Ok(Declaration { specs, declarators, span: self.span_from(start) });
        }
        loop {
            let d = self.parse_declarator(Abstractness::Never)?;
            let is_typedef = matches!(specs.storage, Some((StorageClass::Typedef, _)));
            if let Some(n) = d.name() {
                self.declare(n.name, is_typedef);
            }
            let init = if self.eat_punct(Punct::Eq) { Some(self.parse_initializer()?) } else { None };
            declarators.push(InitDeclarator { declarator: d, init });
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        if self.at_punct(Punct::LBrace) && ctx == SpecCtx::Block {
            self.error_here("function definition is not allowed here")?;
        }
        self.expect_semi("at end of declaration");
        Ok(Declaration { specs, declarators, span: self.span_from(start) })
    }

    pub(crate) fn parse_declaration_or_function(&mut self) -> PResult<Option<ExternalDecl>> {
        let start = self.span();
        let specs = self.parse_decl_specs(SpecCtx::External)?;
        if self.at_punct(Punct::Semi) {
            self.bump();
            return Ok(Some(ExternalDecl::Decl(Declaration {
                specs,
                declarators: Vec::new(),
                span: self.span_from(start),
            })));
        }
        if let Some(kw) = self.missing_semi_after_tag(&specs) {
            let _ = self.error_after_prev(format!("expected ';' after {}", kw), ";");
            return Ok(Some(ExternalDecl::Decl(Declaration {
                specs,
                declarators: Vec::new(),
                span: self.span_from(start),
            })));
        }
        if specs.ty.is_none() && specs.storage.is_none() && !specs.inline && !specs.noreturn && specs.attrs.is_empty() {
            // Not the start of any declaration: garbage at file scope.
            if let PKind::Ident(_) = self.kind() {
                // fallthrough: implicit-int declaration, handled below with a warning
            } else {
                self.error_here("expected identifier or '('")?;
            }
        }
        let is_typedef = matches!(specs.storage, Some((StorageClass::Typedef, _)));
        let mut declarators = Vec::new();
        loop {
            let d = self.parse_declarator(Abstractness::Never)?;
            if declarators.is_empty() && self.at_punct(Punct::LBrace) && d.is_function() {
                if let Some(n) = d.name() {
                    self.declare(n.name, false);
                }
                return self.parse_function_def(start, specs, d).map(|f| Some(ExternalDecl::Func(f)));
            }
            if let Some(n) = d.name() {
                self.declare(n.name, is_typedef);
            }
            let init = if self.eat_punct(Punct::Eq) { Some(self.parse_initializer()?) } else { None };
            declarators.push(InitDeclarator { declarator: d, init });
            if !self.eat_punct(Punct::Comma) {
                break;
            }
        }
        if self.at_punct(Punct::LBrace) {
            self.error_after_prev("expected ';' after top level declarator", ";")?;
        }
        self.expect_semi("after top level declarator");
        Ok(Some(ExternalDecl::Decl(Declaration { specs, declarators, span: self.span_from(start) })))
    }

    fn parse_function_def(&mut self, start: Span, specs: DeclSpecs, declarator: Declarator) -> PResult<FuncDef> {
        self.push_scope();
        if let Some(Suffix::Function { params, .. }) = declarator.suffixes.first() {
            for p in params {
                if let Some(n) = p.declarator.name() {
                    self.declare(n.name, false);
                }
            }
        }
        self.in_function += 1;
        let body = self.parse_compound();
        self.in_function -= 1;
        self.pop_scope();
        let body = body?;
        Ok(FuncDef { specs, declarator, span: self.span_from(start), body })
    }
}

/// `Some("struct" | "union" | "enum")` when the specifier *defines* a tag.
fn tag_keyword(k: &TypeSpecKind) -> Option<&'static str> {
    match k {
        TypeSpecKind::Record(r) if r.members.is_some() => Some(if r.is_union { "union" } else { "struct" }),
        TypeSpecKind::Enum(e) if e.enumerators.is_some() => Some("enum"),
        _ => None,
    }
}

fn storage_name(s: StorageClass) -> &'static str {
    match s {
        StorageClass::Typedef => "typedef",
        StorageClass::Extern => "extern",
        StorageClass::Static => "static",
        StorageClass::Auto => "auto",
        StorageClass::Register => "register",
    }
}
