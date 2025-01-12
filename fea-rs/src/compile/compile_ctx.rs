use std::{
    collections::{BTreeMap, HashMap, HashSet},
    convert::TryInto,
    ops::Range,
};

use smol_str::SmolStr;
use write_fonts::{
    tables::{
        self,
        gdef::CaretValue,
        gpos::{AnchorTable, ValueRecord},
        layout::LookupFlag,
    },
    types::Tag,
};

use crate::{
    common::{GlyphClass, GlyphId, GlyphOrClass},
    parse::SourceMap,
    token_tree::{
        typed::{self, AstNode},
        Token,
    },
    typed::ContextualRuleNode,
    Diagnostic, GlyphIdent, GlyphMap, Kind, NodeOrToken,
};

use super::{
    features::{AaltFeature, ActiveFeature, SizeFeature, SpecialVerticalFeatureState},
    glyph_range,
    language_system::{DefaultLanguageSystems, LanguageSystem},
    lookups::{
        AllLookups, FeatureKey, FilterSetId, LookupFlagInfo, LookupId, PreviouslyAssignedClass,
        SomeLookup,
    },
    output::Compilation,
    tables::{ClassId, CvParams, ScriptRecord, Tables},
    tags,
    valuerecordext::ValueRecordExt,
};

pub struct CompilationCtx<'a> {
    glyph_map: &'a GlyphMap,
    reverse_glyph_map: BTreeMap<GlyphId, GlyphIdent>,
    source_map: &'a SourceMap,
    pub errors: Vec<Diagnostic>,
    tables: Tables,
    features: BTreeMap<FeatureKey, Vec<LookupId>>,
    default_lang_systems: DefaultLanguageSystems,
    lookups: AllLookups,
    lookup_flags: LookupFlagInfo,
    active_feature: Option<ActiveFeature>,
    vertical_feature: SpecialVerticalFeatureState,
    script: Option<Tag>,
    glyph_class_defs: HashMap<SmolStr, GlyphClass>,
    mark_classes: HashMap<SmolStr, MarkClass>,
    anchor_defs: HashMap<SmolStr, (AnchorTable, usize)>,
    mark_attach_class_id: HashMap<GlyphClass, u16>,
    mark_filter_sets: HashMap<GlyphClass, FilterSetId>,
    size: Option<SizeFeature>,
    aalt: Option<AaltFeature>,
    required_features: HashSet<FeatureKey>,
}

#[derive(Clone, Debug, Default)]
struct MarkClass {
    members: Vec<(GlyphClass, Option<AnchorTable>)>,
}

impl<'a> CompilationCtx<'a> {
    pub(crate) fn new(glyph_map: &'a GlyphMap, source_map: &'a SourceMap) -> Self {
        CompilationCtx {
            glyph_map,
            reverse_glyph_map: glyph_map.reverse_map(),
            source_map,
            errors: Vec::new(),
            tables: Tables::default(),
            default_lang_systems: Default::default(),
            glyph_class_defs: Default::default(),
            lookups: Default::default(),
            features: Default::default(),
            mark_classes: Default::default(),
            anchor_defs: Default::default(),
            lookup_flags: Default::default(),
            active_feature: None,
            vertical_feature: Default::default(),
            script: None,
            mark_attach_class_id: Default::default(),
            mark_filter_sets: Default::default(),
            size: None,
            required_features: Default::default(),
            aalt: Default::default(),
        }
    }

    pub(crate) fn compile(&mut self, node: &typed::Root) {
        for item in node.statements() {
            if let Some(language_system) = typed::LanguageSystem::cast(item) {
                self.add_language_system(language_system);
            } else if let Some(class_def) = typed::GlyphClassDef::cast(item) {
                self.define_glyph_class(class_def);
            } else if let Some(mark_def) = typed::MarkClassDef::cast(item) {
                self.define_mark_class(mark_def);
            } else if let Some(anchor_def) = typed::AnchorDef::cast(item) {
                self.define_named_anchor(anchor_def);
            } else if let Some(feature) = typed::Feature::cast(item) {
                self.add_feature(feature);
            } else if let Some(lookup) = typed::LookupBlock::cast(item) {
                self.resolve_lookup_block(lookup);
            } else if item.kind() == Kind::AnonBlockNode {
                // noop
            } else if let Some(table) = typed::Table::cast(item) {
                self.resolve_table(table);
            } else if !item.kind().is_trivia() {
                let span = match item {
                    NodeOrToken::Token(t) => t.range(),
                    NodeOrToken::Node(node) => {
                        let range = node.range();
                        let end = range.end.min(range.start + 16);
                        range.start..end
                    }
                };
                self.error(span, format!("unhandled top-level item: '{}'", item.kind()));
            }
        }

        self.finalize_gdef_table();
        self.finalize_aalt();
        self.sort_and_dedupe_lookups();
    }

    fn sort_and_dedupe_lookups(&mut self) {
        // if any duplicate lookups have made their way into our features, remove them;
        // they will be ignored by the shaper anyway.
        for lookup in self.features.values_mut() {
            // note that the order of lookups in a feature doesn't matter, they
            // are processed in the order that they appear in the lookup list.
            lookup.sort_unstable();
            lookup.dedup();
        }
    }

    fn finalize_aalt(&mut self) {
        let Some(mut aalt) = self.aalt.take() else { return };
        // add all the relevant lookups from the referenced features
        let mut lookups = vec![vec![]; aalt.features().len()];
        // first sort all lookups by the order of the tags in the aalt table:
        for (key, lookup_ids) in &self.features {
            let Some(feat_idx) = aalt.features().iter().position(|tag| *tag == key.feature) else { continue };
            lookups[feat_idx].extend(
                lookup_ids
                    .iter()
                    .flat_map(|idx| self.lookups.aalt_lookups(*idx)),
            )
        }

        // now go through the lookups, ordered by appearance of feature in aalt
        for lookup in lookups.iter().flat_map(|x| x.iter()) {
            match lookup {
                super::lookups::SubstitutionLookup::Single(lookup) => {
                    aalt.extend(lookup.iter_subtables().flat_map(|sub| sub.iter_pairs()))
                }
                super::lookups::SubstitutionLookup::Alternate(lookup) => {
                    aalt.extend(lookup.iter_subtables().flat_map(|sub| sub.iter_pairs()))
                }
                _ => (),
            }
        }

        // now we have all of our referenced lookups, and so we want to use that
        // to construct the aalt lookups:
        let aalt_lookup_indices = self
            .lookups
            .insert_aalt_lookups(std::mem::take(&mut aalt.all_alts));

        // now adjust our previously set lookupids, which are now invalid,
        // since we're going to insert the aalt lookups in front of the lookup
        // list:
        self.features
            .values_mut()
            .flat_map(|x| x.iter_mut())
            .for_each(|id| id.adjust_if_gsub(aalt_lookup_indices.len()));

        // finally add the aalt feature to all the default language systems
        for sys in self.default_lang_systems.iter() {
            self.features
                .insert(sys.to_feature_key(tags::AALT), aalt_lookup_indices.clone());
        }

        self.aalt = Some(aalt);
    }

    pub(crate) fn build(&mut self) -> Result<Compilation, Vec<Diagnostic>> {
        if self.errors.iter().any(Diagnostic::is_error) {
            return Err(self.errors.clone());
        }

        Ok(Compilation {
            warnings: self.errors.clone(),
            lookups: self.lookups.clone(),
            features: self.features.clone(),
            tables: self.tables.clone(),
            size: self.size.clone(),
            required_features: self.required_features.clone(),
        })
    }

    /// Infer/update GDEF table as required.
    ///
    /// If a GDEF table is not explicitly defined, we are supposed to create one,
    /// and even if a GDEF table *is* defined, we are supposed to compute certain
    /// of its subtables based on other items encountered in the feature file
    ///
    /// References:
    ///
    /// <http://adobe-type-tools.github.io/afdko/OpenTypeFeatureFileSpecification.html#4f-markclass>
    /// <http://adobe-type-tools.github.io/afdko/OpenTypeFeatureFileSpecification.html#9b-gdef-table>
    fn finalize_gdef_table(&mut self) {
        // if the FEA included a GDEF block, use that, otherwise create an empty table
        let mut gdef = self.tables.gdef.take().unwrap_or_default();
        // infer glyph classes, if they were not declared explicitly
        if gdef.glyph_classes.is_empty() {
            self.lookups.infer_glyph_classes(|glyph, class_id| {
                gdef.glyph_classes.insert(glyph, class_id);
            });
            for glyph in self
                .mark_classes
                .values()
                .flat_map(|class| class.members.iter().map(|(cls, _)| cls.iter()))
                .flatten()
            {
                gdef.glyph_classes.insert(glyph, ClassId::Mark);
            }
        }

        if !self.mark_attach_class_id.is_empty() {
            gdef.mark_attach_class.extend(
                self.mark_attach_class_id
                    .iter()
                    .flat_map(|(cls, id)| cls.iter().map(|gid| (gid, *id))),
            );
        }

        if !self.mark_filter_sets.is_empty() {
            let mut sorted = self
                .mark_filter_sets
                .iter()
                .map(|(cls, id)| (*id, cls.clone()))
                .collect::<Vec<_>>();
            sorted.sort_unstable();
            gdef.mark_glyph_sets = sorted.into_iter().map(|(_, cls)| cls).collect();
        }

        if !gdef.is_empty() {
            self.tables.gdef = Some(gdef);
        }
    }

    fn error(&mut self, range: Range<usize>, message: impl Into<String>) {
        let (file, range) = self.source_map.resolve_range(range);
        self.errors.push(Diagnostic::error(file, range, message));
    }

    fn warning(&mut self, range: Range<usize>, message: impl Into<String>) {
        let (file, range) = self.source_map.resolve_range(range);
        self.errors.push(Diagnostic::warning(file, range, message));
    }

    fn add_language_system(&mut self, language_system: typed::LanguageSystem) {
        let script = language_system.script().to_raw();
        let language = language_system.language().to_raw();
        self.default_lang_systems
            .insert(LanguageSystem { script, language });
    }

    fn start_feature(&mut self, feature_name: typed::Tag) {
        assert!(
            !self.lookups.has_current(),
            "no lookup should be active at start of feature"
        );
        let raw_tag = feature_name.to_raw();
        self.active_feature = Some(ActiveFeature::new(
            raw_tag,
            self.default_lang_systems.clone(),
        ));
        self.vertical_feature.begin_feature(raw_tag);
        self.lookup_flags.clear();
    }

    fn end_feature(&mut self) {
        if let Some((id, _name)) = self.lookups.finish_current() {
            assert!(
                _name.is_none(),
                "lookup blocks are finished before feature blocks"
            );
            self.add_lookup_to_current_feature_if_present(id);
        }
        let active = self.active_feature.take().expect("always present");
        active.add_to_features(&mut self.features);
        self.vertical_feature.end_feature();
        self.lookup_flags.clear();
    }

    fn start_lookup_block(&mut self, name: &Token) {
        if let Some((id, _name)) = self.lookups.finish_current() {
            assert!(_name.is_none(), "lookup blocks cannot be nested");
            self.add_lookup_to_current_feature_if_present(id);
        }

        if self.active_feature.is_none() {
            self.lookup_flags.clear();
        }

        self.vertical_feature.begin_lookup_block();
        self.lookups.start_named(name.text.clone());
    }

    fn end_lookup_block(&mut self) {
        // end first, regardless of whether we're in an active feature
        let current = self.lookups.finish_current();
        // if this lookup is inside a feature block, it gets added to the feature
        if self.active_feature.is_some() {
            if let Some((id, _)) = current {
                self.add_lookup_to_current_feature_if_present(id);
            }
        // and if not, we clear these flags
        } else {
            self.lookup_flags.clear();
        }
        self.vertical_feature.end_lookup_block();
    }

    fn set_language(&mut self, stmt: typed::Language) {
        let language = stmt.tag().to_raw();
        let script = self.script.unwrap_or(tags::SCRIPT_DFLT);
        self.set_script_language(
            script,
            language,
            stmt.exclude_dflt().is_some(),
            stmt.required().is_some(),
        );
    }

    fn set_script(&mut self, stmt: typed::Script) {
        let script = stmt.tag().to_raw();
        if Some(script) == self.script {
            return;
        }

        self.script = Some(script);
        self.lookup_flags.clear();

        self.set_script_language(script, tags::LANG_DFLT, false, false);
    }

    fn set_script_language(
        &mut self,
        script: Tag,
        language: Tag,
        exclude_dflt: bool,
        required: bool,
    ) {
        let system = LanguageSystem { script, language };
        if let Some((id, _name)) = self.lookups.finish_current() {
            self.add_lookup_to_current_feature_if_present(id);
        }
        let key = self
            .active_feature
            .as_mut()
            .unwrap()
            .set_system(system, exclude_dflt);

        if required {
            self.required_features.insert(key);
        }
    }

    fn set_lookup_flag(&mut self, node: typed::LookupFlag) {
        if let Some(number) = node.number() {
            self.lookup_flags.flags =
                LookupFlag::from_bits_truncate(number.parse_unsigned().unwrap());
            return;
        }

        let mut flags = LookupFlag::empty();
        let mut mark_filter_set = None;

        let mut iter = node.values();
        while let Some(next) = iter.next() {
            match next.kind() {
                Kind::RightToLeftKw => flags.set_right_to_left(true),
                Kind::IgnoreBaseGlyphsKw => flags.set_ignore_base_glyphs(true),
                Kind::IgnoreLigaturesKw => flags.set_ignore_ligatures(true),
                Kind::IgnoreMarksKw => flags.set_ignore_marks(true),

                //FIXME: we are not enforcing some requirements here. in particular,
                // The glyph sets of the referenced classes must not overlap, and the MarkAttachmentType statement can reference at most 15 different classes.
                // ALSO: this should accept mark classes.
                Kind::MarkAttachmentTypeKw => {
                    let node = iter
                        .next()
                        .and_then(typed::GlyphClass::cast)
                        .expect("validated");
                    let mark_attach_set = self.resolve_mark_attach_class(&node);
                    flags.set_mark_attachment_type(mark_attach_set);
                }
                Kind::UseMarkFilteringSetKw => {
                    let node = iter
                        .next()
                        .and_then(typed::GlyphClass::cast)
                        .expect("validated");
                    let filter_set = self.resolve_mark_filter_set(&node);
                    flags.set_use_mark_filtering_set(true);
                    mark_filter_set = Some(filter_set);
                }
                other => unreachable!("mark statements have been validated: '{:?}'", other),
            }
        }
        self.lookup_flags = LookupFlagInfo::new(flags, mark_filter_set);
    }

    fn resolve_mark_attach_class(&mut self, glyphs: &typed::GlyphClass) -> u16 {
        let glyphs = self.resolve_glyph_class(glyphs);
        let mark_set = glyphs.sort_and_dedupe();
        if let Some(id) = self.mark_attach_class_id.get(&mark_set) {
            return *id;
        }

        let id = self.mark_attach_class_id.len() as u16 + 1;
        //FIXME: I don't understand what is not allowed here

        self.mark_attach_class_id.insert(mark_set, id);
        id
    }

    fn resolve_mark_filter_set(&mut self, glyphs: &typed::GlyphClass) -> u16 {
        let glyphs = self.resolve_glyph_class(glyphs);
        let set = glyphs.sort_and_dedupe();
        let id = self.mark_filter_sets.len();
        *self
            .mark_filter_sets
            .entry(set)
            .or_insert_with(|| id.try_into().unwrap())
    }

    pub fn add_subtable_break(&mut self) {
        if !self.lookups.add_subtable_break() {
            //TODO: report that we weren't in a lookup?
        }
    }

    fn ensure_current_lookup_type(&mut self, kind: Kind) -> &mut SomeLookup {
        if self.lookups.needs_new_lookup(kind) {
            //FIXME: find another way of ensuring that named lookup blocks don't
            //contain mismatched rules
            //assert!(!self.lookups.is_named(), "ensure rule type in validation");
            if let Some(lookup) = self.lookups.start_lookup(kind, self.lookup_flags) {
                self.add_lookup_to_current_feature_if_present(lookup);
            }
        }
        self.lookups.current_mut().expect("we just created it")
    }

    fn add_lookup_to_current_feature_if_present(&mut self, lookup: LookupId) {
        if lookup != LookupId::Empty {
            if let Some(active) = self.active_feature.as_mut() {
                active.add_lookup(lookup);
            }
        }
    }

    fn add_gpos_statement(&mut self, node: typed::GposStatement) {
        match node {
            typed::GposStatement::Type1(rule) => self.add_single_pos(&rule),
            typed::GposStatement::Type2(rule) => self.add_pair_pos(&rule),
            typed::GposStatement::Type3(rule) => self.add_cursive_pos(&rule),
            typed::GposStatement::Type4(rule) => self.add_mark_to_base(&rule),
            typed::GposStatement::Type5(rule) => self.add_mark_to_lig(&rule),
            typed::GposStatement::Type6(rule) => self.add_mark_to_mark(&rule),
            typed::GposStatement::Type8(rule) => self.add_contextual_pos_rule(&rule),
            typed::GposStatement::Ignore(rule) => self.add_contextual_pos_ignore(&rule),
        }
    }

    fn add_gsub_statement(&mut self, node: typed::GsubStatement) {
        match node {
            typed::GsubStatement::Type1(rule) => self.add_single_sub(&rule),
            typed::GsubStatement::Type2(rule) => self.add_multiple_sub(&rule),
            typed::GsubStatement::Type3(rule) => self.add_alternate_sub(&rule),
            typed::GsubStatement::Type4(rule) => self.add_ligature_sub(&rule),
            typed::GsubStatement::Type6(rule) => self.add_contextual_sub(&rule),
            typed::GsubStatement::Ignore(rule) => self.add_contextual_sub_ignore(&rule),
            typed::GsubStatement::Type8(rule) => self.add_reverse_contextual_sub(&rule),
            _ => self.warning(node.range(), "unimplemented rule type"),
        }
    }

    fn add_single_sub(&mut self, node: &typed::Gsub1) {
        if let Some((target, replacement)) = self.resolve_single_sub_glyphs(node) {
            if replacement.is_null() {
                // when the replacement is null, it means we are 'deleting' a glyph
                // which uses a trick: we represent it as a multiple substitution
                // rule, with the target sequence being empty.
                // This is explicitly forbidden in the OpenType spec, and
                // explicitly encouraged in the FEA spec, and everyone else does it.
                // see https://github.com/adobe-type-tools/afdko/issues/1438
                let lookup = self.ensure_current_lookup_type(Kind::GsubType2);
                for target in target.iter() {
                    lookup.add_gsub_type_2(target, vec![]);
                }
            } else {
                let lookup = self.ensure_current_lookup_type(Kind::GsubType1);
                for (target, replacement) in target.iter().zip(replacement.into_iter_for_target()) {
                    lookup.add_gsub_type_1(target, replacement);
                }
            }
        }
    }

    fn resolve_single_sub_glyphs(
        &mut self,
        node: &typed::Gsub1,
    ) -> Option<(GlyphOrClass, GlyphOrClass)> {
        self.validate_single_sub_inputs(&node.target(), node.replacement().as_ref())
    }
    //TODO: this should be in validate, but we don't have access to resolved
    //glyphs there right now :(
    fn validate_single_sub_inputs(
        &mut self,
        target: &typed::GlyphOrClass,
        replace: Option<&typed::GlyphOrClass>,
    ) -> Option<(GlyphOrClass, GlyphOrClass)> {
        let target_ids = self.resolve_glyph_or_class(target);
        let replace_ids = replace
            .map(|r| self.resolve_glyph_or_class(r))
            .unwrap_or(GlyphOrClass::Null);
        match (target_ids, replace_ids) {
            (GlyphOrClass::Null, _) => {
                self.error(target.range(), "NULL is not a valid substitution target");
                None
            }
            (GlyphOrClass::Glyph(_), GlyphOrClass::Class(_)) => {
                self.error(replace.unwrap().range(), "cannot sub glyph by glyph class");
                None
            }
            (GlyphOrClass::Class(c1), GlyphOrClass::Class(c2)) if c1.len() != c2.len() => {
                self.error(
                    replace.unwrap().range(),
                    format!(
                        "class has different length ({}) than target ({})",
                        c1.len(),
                        c2.len()
                    ),
                );
                None
            }
            other => Some(other),
        }
    }

    fn add_multiple_sub(&mut self, node: &typed::Gsub2) {
        let target = node.target();
        let target_id = self.resolve_glyph(&target);
        let replacement = node.replacement().map(|g| self.resolve_glyph(&g)).collect();
        let lookup = self.ensure_current_lookup_type(Kind::GsubType2);
        lookup.add_gsub_type_2(target_id, replacement);
    }

    fn add_alternate_sub(&mut self, node: &typed::Gsub3) {
        let target = self.resolve_glyph(&node.target());
        let alts = self.resolve_glyph_class(&node.alternates());
        let lookup = self.ensure_current_lookup_type(Kind::GsubType3);
        lookup.add_gsub_type_3(target, alts.iter().collect());
    }

    fn add_ligature_sub(&mut self, node: &typed::Gsub4) {
        let target = node
            .target()
            .map(|g| self.resolve_glyph_or_class(&g))
            .collect::<Vec<_>>();
        let replacement = self.resolve_glyph(&node.replacement());
        let lookup = self.ensure_current_lookup_type(Kind::GsubType4);

        for target in sequence_enumerator(&target) {
            lookup.add_gsub_type_4(target, replacement);
        }
    }

    fn add_contextual_sub(&mut self, node: &typed::Gsub6) {
        let backtrack = self.resolve_backtrack_sequence(node.backtrack().items());
        let lookahead = self.resolve_lookahead_sequence(node.lookahead().items());
        // does this have an inline rule?
        let mut inline = node.inline_rule().and_then(|rule| {
            let input = node.input();
            if input.items().nth(1).is_some() {
                // more than one input: this is a ligature rule
                let target = input
                    .items()
                    .map(|inp| self.resolve_glyph_or_class(&inp.target()))
                    .collect::<Vec<_>>();
                let replacement = self.resolve_glyph(&rule.replacement_glyphs().next().unwrap());
                let lookup = self.ensure_current_lookup_type(Kind::GsubType6);
                //FIXME: we should check that the whole sequence is not present the
                //lookup before adding..
                let mut to_return = None;
                for target in sequence_enumerator(&target) {
                    to_return = Some(
                        lookup
                            .as_gsub_contextual()
                            .add_anon_gsub_type_4(target, replacement),
                    );
                }
                to_return
            } else {
                let target = input.items().next().unwrap().target();
                let replacement = rule.replacements().next().unwrap();
                if let Some((target, replacement)) =
                    self.validate_single_sub_inputs(&target, Some(&replacement))
                {
                    let lookup = self.ensure_current_lookup_type(Kind::GsubType6);
                    Some(
                        lookup
                            .as_gsub_contextual()
                            .add_anon_gsub_type_1(target, replacement),
                    )
                } else {
                    None
                }
            }
        });

        let context = node
            .input()
            .items()
            .map(|item| {
                let glyphs = self.resolve_glyph_or_class(&item.target());
                let mut lookups = Vec::new();
                // if there's an inline rule it always belongs to the first marked
                // glyph, so this should work? it may need to change for fancier
                // inline rules in the future.
                if let Some(inline) = inline.take() {
                    lookups.push(inline);
                }

                for lookup in item.lookups() {
                    let id = self.lookups.get_named(&lookup.label().text).unwrap(); // validated already
                    if matches!(id, LookupId::Gpos(_)) {
                        self.error(
                            lookup.label().range(),
                            "Invalid lookup: expected GSUB, found GPOS",
                        );
                    }
                    lookups.push(id);
                }
                (glyphs, lookups)
            })
            .collect::<Vec<_>>();

        let lookup = self.ensure_current_lookup_type(Kind::GsubType6);
        lookup.add_contextual_rule(backtrack, context, lookahead);
    }

    fn add_contextual_sub_ignore(&mut self, node: &typed::GsubIgnore) {
        for rule in node.rules() {
            self.add_contextual_ignore_rule(&rule, Kind::GsubType6);
        }
    }

    fn add_reverse_contextual_sub(&mut self, node: &typed::Gsub8) {
        let backtrack = self.resolve_backtrack_sequence(node.backtrack().items());
        let lookahead = self.resolve_lookahead_sequence(node.lookahead().items());
        let input = node.input().items().next().unwrap();
        let target = input.target();
        let replacement = node.inline_rule().and_then(|r| r.replacements().next());
        //FIXME: warn if there are actual lookups here, we don't support that
        if let Some((target, replacement)) =
            self.validate_single_sub_inputs(&target, replacement.as_ref())
        {
            let context = target
                .iter()
                .zip(replacement.into_iter_for_target())
                .collect();
            self.ensure_current_lookup_type(Kind::GsubType8)
                .add_gsub_type_8(backtrack, context, lookahead);
        }
    }

    fn add_single_pos(&mut self, node: &typed::Gpos1) {
        let ids = self.resolve_glyph_or_class(&node.target());
        let record = self.resolve_value_record(&node.value());
        let lookup = self.ensure_current_lookup_type(Kind::GposType1);
        for id in ids.iter() {
            lookup.add_gpos_type_1(id, record.clone());
        }
    }

    fn add_pair_pos(&mut self, node: &typed::Gpos2) {
        let in_vert_feature = self.vertical_feature.in_eligible_vertical_feature();

        let first_ids = self.resolve_glyph_or_class(&node.first_item());
        let second_ids = self.resolve_glyph_or_class(&node.second_item());
        let first_value = self
            .resolve_value_record_raw(&node.first_value())
            .for_pair_pos(in_vert_feature);
        let second_value = node
            .second_value()
            .map(|val| self.resolve_value_record_raw(&val))
            .unwrap_or_default()
            .for_pair_pos(in_vert_feature);

        let lookup = self.ensure_current_lookup_type(Kind::GposType2);

        if (first_ids.is_class() || second_ids.is_class()) && node.enum_().is_none() {
            lookup.add_gpos_type_2_class(
                first_ids.to_class().unwrap(),
                second_ids.to_class().unwrap(),
                first_value,
                second_value,
            )
        } else {
            for first in first_ids.iter() {
                for second in second_ids.iter() {
                    lookup.add_gpos_type_2_pair(
                        first,
                        second,
                        first_value.clone(),
                        second_value.clone(),
                    );
                }
            }
        }
    }

    fn add_cursive_pos(&mut self, node: &typed::Gpos3) {
        let ids = self.resolve_glyph_or_class(&node.target());
        // if null it means we've already reported an error and compilation
        // will fail.
        let entry = self.resolve_anchor(&node.entry());
        let exit = self.resolve_anchor(&node.exit());
        let lookup = self.ensure_current_lookup_type(Kind::GposType3);
        for id in ids.iter() {
            lookup.add_gpos_type_3(id, entry.clone(), exit.clone())
        }
    }

    fn add_mark_to_base(&mut self, node: &typed::Gpos4) {
        let base_ids = self.resolve_glyph_or_class(&node.base());
        let _ = self.ensure_current_lookup_type(Kind::GposType4);
        for mark in node.attachments() {
            let base_anchor = self.resolve_anchor(&mark.anchor());

            let mark_class_node = mark.mark_class_name().expect("checked in validation");
            let class_name = mark_class_node.text().to_owned();
            let mark_class = self.mark_classes.get(&class_name).unwrap();

            // access the lookup through the field, so the borrow checker
            // doesn't think we're borrowing all of self
            //TODO: we do validation here because our validation pass isn't smart
            //enough. We need to not just validate a rule, but every rule in a lookup.
            let maybe_err = self
                .lookups
                .current_mut()
                .unwrap()
                .with_gpos_type_4(|subtable| {
                    for (glyphs, mark_anchor) in &mark_class.members {
                        let anchor = mark_anchor
                            .as_ref()
                            .expect("no null anchors in mark-to-base (check validation)");
                        for glyph in glyphs.iter() {
                            subtable.insert_mark(glyph, class_name.clone(), anchor.clone())?;
                        }
                    }
                    for base in base_ids.iter() {
                        subtable.insert_base(
                            base,
                            &class_name,
                            base_anchor
                                .clone()
                                .expect("no null anchors in mark-to-base"),
                        )
                    }
                    Ok(())
                });
            self.maybe_report_mark_class_conflict(mark_class_node.range(), maybe_err.err())
        }
    }

    fn add_mark_to_lig(&mut self, node: &typed::Gpos5) {
        let base_ids = self.resolve_glyph_or_class(&node.base());
        // okay so:
        // for each lig glyph in the input, we create a lig array table.
        // for each component in each lig glyph, we add a component record
        // to that lig glyph table.
        // for each anchor point in each component, we add an anchor record
        // to that component

        let mut components = Vec::new();
        for component in node.ligature_components() {
            let _lookup = self.ensure_current_lookup_type(Kind::GposType5);

            let mut anchor_records = BTreeMap::new();
            for attachment in component.attachments() {
                let component_anchor = self.resolve_anchor(&attachment.anchor());
                let mark_class_node = match attachment.mark_class_name() {
                    Some(node) => node,
                    None => {
                        // this means that there is a single null anchor for this
                        // component, which in turn means that there are no
                        // attachment points. we will fill them in later.
                        assert!(component_anchor.is_none());
                        continue;
                    }
                };
                let component_anchor = component_anchor.unwrap();
                let class_name = mark_class_node.text();
                let mark_class = self.mark_classes.get(class_name).unwrap();

                // access the lookup through the field, so the borrow checker
                // doesn't think we're borrowing all of self
                //TODO: we do validation here because our validation pass isn't smart
                //enough. We need to not just validate a rule, but every rule in a lookup.
                anchor_records.insert(class_name.clone(), component_anchor);
                let maybe_err = self
                    .lookups
                    .current_mut()
                    .unwrap()
                    .with_gpos_type_5(|subtable| {
                        for (glyphs, mark_anchor) in &mark_class.members {
                            let anchor = mark_anchor
                                .as_ref()
                                .expect("no null anchors on marks (check validation)");
                            for glyph in glyphs.iter() {
                                subtable.insert_mark(glyph, class_name.clone(), anchor.clone())?;
                            }
                        }
                        Ok(())
                    });
                self.maybe_report_mark_class_conflict(mark_class_node.range(), maybe_err.err());
            }
            components.push(anchor_records);
        }

        self.lookups
            .current_mut()
            .unwrap()
            .with_gpos_type_5(|subtable| {
                for base in base_ids.iter() {
                    subtable.add_lig(base, components.clone());
                }
            })
    }

    //FIXME: this is basically identical to type 4, but the validation stuff
    //makes it all a big PITA. when we have better validation, we can probably improve this
    //significantly.
    fn add_mark_to_mark(&mut self, node: &typed::Gpos6) {
        let base_ids = self.resolve_glyph_or_class(&node.base());
        let _ = self.ensure_current_lookup_type(Kind::GposType6);
        for mark in node.attachments() {
            let base_anchor = self.resolve_anchor(&mark.anchor());
            let mark_class_node = mark.mark_class_name().expect("checked in validation");
            let class_name = mark_class_node.text();
            let mark_class = self.mark_classes.get(mark_class_node.text()).unwrap();

            //TODO: we do validation here because our validation pass isn't smart
            //enough. We need to not just validate a rule, but every rule in a lookup.
            let maybe_err = self
                .lookups
                .current_mut()
                .unwrap()
                .with_gpos_type_6(|subtable| {
                    for (glyphs, mark_anchor) in &mark_class.members {
                        let anchor = mark_anchor
                            .as_ref()
                            .expect("no null anchors in mark-to-mark (check validation)");
                        for glyph in glyphs.iter() {
                            subtable.insert_mark(glyph, class_name.clone(), anchor.clone())?;
                        }
                    }
                    for base in base_ids.iter() {
                        subtable.insert_base(
                            base,
                            class_name,
                            base_anchor
                                .clone()
                                .expect("no null anchors in mark-to-mark"),
                        );
                    }
                    Ok(())
                });
            self.maybe_report_mark_class_conflict(mark_class_node.range(), maybe_err.err())
        }
    }

    fn maybe_report_mark_class_conflict(
        &mut self,
        range: Range<usize>,
        maybe_err: Option<PreviouslyAssignedClass>,
    ) {
        if let Some(PreviouslyAssignedClass { class, .. }) = maybe_err {
            self.error(
                range,
                format!("mark class includes glyph in class '{class}', already used in lookup.",),
            );
        };
    }

    fn add_contextual_pos_rule(&mut self, node: &typed::Gpos8) {
        let backtrack = self.resolve_backtrack_sequence(node.backtrack().items());
        let lookahead = self.resolve_lookahead_sequence(node.lookahead().items());
        let context = node
            .input()
            .items()
            .map(|item| {
                let glyphs = self.resolve_glyph_or_class(&item.target());
                let mut lookups = Vec::new();
                if let Some(value) = item.valuerecord() {
                    let value = self.resolve_value_record(&value);
                    let anon_id = self
                        .ensure_current_lookup_type(Kind::GposType8)
                        .as_gpos_contextual()
                        .add_anon_gpos_type_1(&glyphs, value);
                    lookups.push(anon_id);
                }

                for lookup in item.lookups() {
                    let id = self.lookups.get_named(&lookup.label().text).unwrap();
                    if matches!(id, LookupId::Gsub(_)) {
                        self.error(
                            lookup.label().range(),
                            "Invalid lookup type: expected GPOS, found GSUB",
                        );
                    }
                    lookups.push(id);
                }

                (glyphs, lookups)
            })
            .collect();
        self.ensure_current_lookup_type(Kind::GposType8)
            .add_contextual_rule(backtrack, context, lookahead);
    }

    fn add_contextual_pos_ignore(&mut self, node: &typed::GposIgnore) {
        for rule in node.rules() {
            self.add_contextual_ignore_rule(&rule, Kind::GposType8);
        }
    }

    fn add_contextual_ignore_rule(&mut self, rule: &typed::IgnoreRule, kind: Kind) {
        let backtrack = self.resolve_backtrack_sequence(rule.backtrack().items());
        let lookahead = self.resolve_lookahead_sequence(rule.lookahead().items());
        let context = rule
            .input()
            .items()
            .map(|item| (self.resolve_glyph_or_class(&item.target()), Vec::new()))
            .collect();
        let lookup = self.ensure_current_lookup_type(kind);
        lookup.add_contextual_rule(backtrack, context, lookahead);
    }

    /// Resolve a value record, ignoring zero values
    ///
    /// This is the default behaviour; a value record of '0' or <0 0 0 0> has
    /// format zero.
    fn resolve_value_record(&mut self, record: &typed::ValueRecord) -> ValueRecord {
        self.resolve_value_record_raw(record).clear_zeros()
    }

    /// Resolve a value record, leaving zeros in place
    ///
    /// This is exposed to handle PairPos, which has special semantics for how
    /// to interpret and handle zeros.
    fn resolve_value_record_raw(&mut self, record: &typed::ValueRecord) -> ValueRecord {
        if record.null().is_some() {
            return ValueRecord::default();
        }

        if let Some(adv) = record.advance().map(|x| x.parse_signed()) {
            let (x_advance, y_advance) = if self.vertical_feature.in_eligible_vertical_feature() {
                (None, Some(adv))
            } else {
                (Some(adv), None)
            };

            return ValueRecord {
                x_advance,
                y_advance,
                ..Default::default()
            };
        }
        if let Some([x_place, y_place, x_adv, y_adv]) = record.placement() {
            let mut result = ValueRecord {
                x_advance: Some(x_adv.parse_signed()),
                y_advance: Some(y_adv.parse_signed()),
                x_placement: Some(x_place.parse_signed()),
                y_placement: Some(y_place.parse_signed()),
                ..Default::default()
            };
            if let Some([x_place_dev, y_place_dev, x_adv_dev, y_adv_dev]) = record.device() {
                result.x_placement_device.set(x_place_dev.compile());
                result.y_placement_device.set(y_place_dev.compile());
                result.x_advance_device.set(x_adv_dev.compile());
                result.y_advance_device.set(y_adv_dev.compile());
            }
            return result;
        }
        if let Some(name) = record.named() {
            //FIXME:
            self.warning(name.range(), "named value records not implemented yet");
        }

        ValueRecord::default()
    }

    fn define_glyph_class(&mut self, class_decl: typed::GlyphClassDef) {
        let name = class_decl.class_name();
        let glyphs = if let Some(class) = class_decl.class_def() {
            self.resolve_glyph_class_literal(&class)
        } else if let Some(alias) = class_decl.class_alias() {
            self.resolve_named_glyph_class(&alias)
        } else {
            panic!("write more code I guess");
        };

        self.glyph_class_defs.insert(name.text().clone(), glyphs);
    }

    fn define_mark_class(&mut self, class_decl: typed::MarkClassDef) {
        let class_items = class_decl.glyph_class();
        let class_items = self.resolve_glyph_or_class(&class_items).into();

        let anchor = self.resolve_anchor(&class_decl.anchor());
        let class_name = class_decl.mark_class_name();
        self.mark_classes
            .entry(class_name.text().clone())
            .or_default()
            .members
            .push((class_items, anchor));
    }

    fn add_feature(&mut self, feature: typed::Feature) {
        let tag = feature.tag();
        let tag_raw = tag.to_raw();
        self.start_feature(tag);
        if tag_raw == tags::AALT {
            self.resolve_aalt_feature(&feature);
        } else if tag_raw == tags::SIZE {
            self.resolve_size_feature(&feature);
        } else if tags::is_stylistic_set(tag_raw) {
            self.resolve_stylistic_set_feature(tag_raw, &feature);
        } else if tags::is_character_variant(tag_raw) {
            self.resolve_character_variant_feature(tag_raw, &feature);
        } else {
            for item in feature.statements() {
                self.resolve_statement(item);
            }
        }
        self.end_feature();
    }

    fn resolve_aalt_feature(&mut self, feature: &typed::Feature) {
        let mut aalt = AaltFeature::default();
        for item in feature.statements() {
            if let Some(node) = typed::Gsub1::cast(item) {
                let Some((target, replacement)) = self.resolve_single_sub_glyphs(&node) else { continue };
                aalt.extend(target.iter().zip(replacement.into_iter_for_target()))
            } else if let Some(node) = typed::Gsub3::cast(item) {
                let target = self.resolve_glyph(&node.target());
                let alts = self.resolve_glyph_class(&node.alternates());
                aalt.extend(std::iter::repeat(target).zip(alts.iter()));
            } else if let Some(feature) = typed::FeatureRef::cast(item) {
                aalt.add_feature_reference(feature.feature().to_raw());
            }
        }
        self.aalt = Some(aalt);
    }

    fn resolve_stylistic_set_feature(&mut self, tag: Tag, feature: &typed::Feature) {
        let mut names = Vec::new();
        if let Some(feature_name) = feature.stylistic_set_feature_names() {
            for name_spec in feature_name.statements() {
                let resolved = self.resolve_name_spec(&name_spec);
                names.push(resolved);
            }
        }
        if !names.is_empty() {
            self.tables.stylistic_sets.insert(tag, names);
        }
        for item in feature
            .statements()
            .filter(|node| node.kind() != Kind::FeatureNamesKw)
        {
            self.resolve_statement(item);
        }
    }

    fn resolve_character_variant_feature(&mut self, tag: Tag, feature: &typed::Feature) {
        if let Some(cv_params) = feature.character_variant_params() {
            let mut params = CvParams::default();
            if let Some(node) = cv_params.feat_ui_label_name() {
                params.feat_ui_label_name = node
                    .statements()
                    .map(|x| self.resolve_name_spec(&x))
                    .collect();
            }
            if let Some(node) = cv_params.feat_tooltip_text_name() {
                params.feat_ui_tooltip_text_name = node
                    .statements()
                    .map(|x| self.resolve_name_spec(&x))
                    .collect();
            }
            if let Some(node) = cv_params.sample_text_name() {
                params.samle_text_name = node
                    .statements()
                    .map(|x| self.resolve_name_spec(&x))
                    .collect();
            }
            if let Some(node) = cv_params.sample_text_name() {
                params.samle_text_name = node
                    .statements()
                    .map(|x| self.resolve_name_spec(&x))
                    .collect();
            }
            for node in cv_params.param_ui_label_name() {
                params.param_ui_label_names.push(
                    node.statements()
                        .map(|x| self.resolve_name_spec(&x))
                        .collect(),
                );
            }
            for c in cv_params.characters() {
                params.characters.push(c.value().parse_char().unwrap());
            }

            self.tables.character_variants.insert(tag, params);
        }

        for item in feature
            .statements()
            .filter(|node| node.kind() != Kind::CvParametersKw)
        {
            self.resolve_statement(item);
        }
    }

    fn resolve_size_feature(&mut self, feature: &typed::Feature) {
        //FIXME: I thought this was signed, but I now think it's
        // unsigned? Double check with spec and ensure this is validated
        fn resolve_decipoint(node: &typed::FloatLike) -> u16 {
            match node {
                typed::FloatLike::Number(n) => n.parse_unsigned(),
                typed::FloatLike::Float(f) => {
                    let f = f.parse();
                    ((f * 10.0).round() as i16).try_into().ok()
                }
            }
            .expect("validated at parse time")
        }

        let mut size = SizeFeature::default();
        for statement in feature.statements() {
            if let Some(node) = typed::SizeMenuName::cast(statement) {
                size.names.push(self.resolve_name_spec(&node.spec()));
            } else if let Some(node) = typed::Parameters::cast(statement) {
                size.design_size = resolve_decipoint(&node.design_size());
                size.identifier = node.subfamily().parse_unsigned().unwrap();
                if size.identifier != 0 {
                    size.range_start = resolve_decipoint(&node.range_start().unwrap());
                    size.range_end = resolve_decipoint(&node.range_end().unwrap());
                }
            }
        }
        for sys in self.default_lang_systems.iter() {
            let key = sys.to_feature_key(tags::SIZE);
            self.features.entry(key).or_default();
        }
        self.size = Some(size);
    }

    fn resolve_table(&mut self, table: typed::Table) {
        match table {
            typed::Table::Base(table) => self.resolve_base(&table),
            typed::Table::Hhea(table) => self.resolve_hhea(&table),
            typed::Table::Vhea(table) => self.resolve_vhea(&table),
            typed::Table::Vmtx(table) => self.resolve_vmtx(&table),
            typed::Table::Name(table) => self.resolve_name(&table),
            typed::Table::Gdef(table) => self.resolve_gdef(&table),
            typed::Table::Head(table) => self.resolve_head(&table),
            typed::Table::Os2(table) => self.resolve_os2(&table),
            typed::Table::Stat(table) => self.resolve_stat(&table),
            _ => (),
        }
    }

    fn resolve_base(&mut self, table: &typed::BaseTable) {
        let mut base = super::tables::Base::default();
        if let Some(list) = table.horiz_base_tag_list() {
            base.horiz_tag_list = list.tags().map(|t| t.to_raw()).collect();
        }
        if let Some(list) = table.horiz_base_script_record_list() {
            base.horiz_script_list = list
                .script_records()
                .map(|record| ScriptRecord {
                    script: record.script().to_raw(),
                    default_baseline_tag: record.default_baseline().to_raw(),
                    values: record.values().map(|i| i.parse_signed()).collect(),
                })
                .collect();
            base.horiz_script_list
                .sort_unstable_by_key(|rec| rec.script);
        }

        if let Some(list) = table.vert_base_tag_list() {
            base.vert_tag_list = list.tags().map(|t| t.to_raw()).collect();
        }
        if let Some(list) = table.vert_base_script_record_list() {
            base.vert_script_list = list
                .script_records()
                .map(|record| ScriptRecord {
                    script: record.script().to_raw(),
                    default_baseline_tag: record.default_baseline().to_raw(),
                    values: record.values().map(|i| i.parse_signed()).collect(),
                })
                .collect();
            base.vert_script_list.sort_unstable_by_key(|rec| rec.script);
        }
        self.tables.base = Some(base);
    }

    fn resolve_name(&mut self, table: &typed::NameTable) {
        for record in table.statements() {
            let name_id = record.name_id().parse().unwrap();
            let spec = self.resolve_name_spec(&record.entry());
            self.tables.name.add(name_id, spec);
        }
    }

    fn resolve_os2(&mut self, table: &typed::Os2Table) {
        let mut os2 = super::tables::Os2Builder::default();
        for item in table.statements() {
            match item {
                typed::Os2TableItem::Number(val) => {
                    let value = val.number().parse_unsigned().unwrap();
                    match val.keyword().text.as_str() {
                        "WeightClass" => os2.us_weight_class = value,
                        "WidthClass" => os2.us_width_class = value,
                        "LowerOpSize" => os2.us_lower_optical_point_size = Some(value),
                        "UpperOpSize" => os2.us_upper_optical_point_size = Some(value),
                        "FSType" => os2.fs_type = value,
                        _ => unreachable!("checked at parse time"),
                    }
                }
                typed::Os2TableItem::Metric(val) => {
                    let value = val.metric().parse();
                    match val.keyword().kind {
                        Kind::TypoAscenderKw => os2.s_typo_ascender = value,
                        Kind::TypoDescenderKw => os2.s_typo_descender = value,
                        Kind::TypoLineGapKw => os2.s_typo_line_gap = value,
                        Kind::XHeightKw => os2.sx_height = value,
                        Kind::CapHeightKw => os2.s_cap_height = value,
                        Kind::WinAscentKw => os2.us_win_ascent = value as u16,
                        Kind::WinDescentKw => os2.us_win_descent = value as u16,
                        _ => unreachable!("checked at parse time"),
                    }
                }
                typed::Os2TableItem::NumberList(list) => match list.keyword().kind {
                    Kind::PanoseKw => {
                        for (i, val) in list.values().enumerate() {
                            os2.panose_10[i] = val.parse_signed() as u8;
                        }
                    }
                    Kind::UnicodeRangeKw => {
                        for val in list.values() {
                            os2.unicode_range.set_bit(val.parse_signed() as _);
                        }
                    }
                    Kind::CodePageRangeKw => {
                        for val in list.values() {
                            os2.code_page_range
                                .add_code_page(val.parse_unsigned().unwrap());
                        }
                    }
                    _ => unreachable!("checked at parse time"),
                },
                typed::Os2TableItem::Vendor(item) => {
                    os2.ach_vend_id = Tag::new(item.value().text.trim_matches('"').as_bytes());
                }
                typed::Os2TableItem::FamilyClass(item) => {
                    os2.s_family_class = item.value().parse().unwrap() as i16
                }
            }
        }
        self.tables.os2 = Some(os2);
    }

    fn resolve_stat(&mut self, table: &typed::StatTable) {
        let mut stat = super::tables::StatBuilder {
            name: super::tables::StatFallbackName::Id(u16::MAX),
            records: Vec::new(),
            values: Vec::new(),
        };

        for item in table.statements() {
            match item {
                typed::StatTableItem::ElidedFallbackName(name) => {
                    if let Some(id) = name.elided_fallback_name_id() {
                        //FIXME: validate
                        stat.name =
                            super::tables::StatFallbackName::Id(id.parse_unsigned().unwrap());
                    } else {
                        let names = name.names().map(|n| self.resolve_name_spec(&n)).collect();
                        stat.name = super::tables::StatFallbackName::Record(names);
                    }
                }
                typed::StatTableItem::AxisValue(value) => {
                    stat.values.push(self.resolve_stat_axis_value(&value));
                }
                typed::StatTableItem::DesignAxis(value) => {
                    let tag = value.tag().to_raw();
                    let ordering = value.ordering().parse_unsigned().unwrap();
                    //FIXME: validate
                    let name = value.names().map(|n| self.resolve_name_spec(&n)).collect();
                    stat.records.push(super::tables::AxisRecord {
                        tag,
                        ordering,
                        name,
                    });
                }
            }
        }
        self.tables.stat = Some(stat);
    }

    fn resolve_stat_axis_value(&mut self, node: &typed::StatAxisValue) -> super::tables::AxisValue {
        use super::tables::AxisLocation;
        let mut flags = 0;
        let mut name = Vec::new();
        let mut location = None;
        for item in node.statements() {
            match item {
                typed::StatAxisValueItem::Flag(flag) => {
                    for bit in flag.bits() {
                        flags |= bit;
                    }
                }
                typed::StatAxisValueItem::NameRecord(record) => {
                    name.push(self.resolve_name_spec(&record));
                }
                typed::StatAxisValueItem::Location(loc) => {
                    let loc_tag = loc.tag().to_raw();
                    match loc.value() {
                        typed::LocationValue::Value(num) => {
                            let val = num.parse_fixed();
                            if let Some(AxisLocation::One { tag, value }) = location.as_ref() {
                                location = Some(AxisLocation::Four(vec![(*tag, *value)]));
                            }
                            if let Some(AxisLocation::Four(vals)) = location.as_mut() {
                                vals.push((loc_tag, val));
                            } else {
                                location = Some(AxisLocation::One {
                                    tag: loc_tag,
                                    value: val,
                                });
                            }
                        }
                        typed::LocationValue::MinMax { nominal, min, max } => {
                            location = Some(AxisLocation::Two {
                                tag: loc_tag,
                                nominal: nominal.parse_fixed(),
                                max: max.parse_fixed(),
                                min: min.parse_fixed(),
                            });
                        }
                        typed::LocationValue::Linked { value, linked } => {
                            location = Some(AxisLocation::Three {
                                tag: loc_tag,
                                value: value.parse_fixed(),
                                linked: linked.parse_fixed(),
                            });
                        }
                    }
                }
            }
        }

        super::tables::AxisValue {
            flags,
            name,
            location: location.unwrap(),
        }
    }

    fn resolve_hhea(&mut self, table: &typed::HheaTable) {
        let mut hhea = tables::hhea::Hhea::default();
        for record in table.metrics() {
            let keyword = record.keyword();
            match keyword.kind {
                Kind::CaretOffsetKw => hhea.caret_offset = record.metric().parse(),
                Kind::AscenderKw => hhea.ascender = record.metric().parse().into(),
                Kind::DescenderKw => hhea.descender = record.metric().parse().into(),
                Kind::LineGapKw => hhea.line_gap = record.metric().parse().into(),
                other => panic!("bug in parser, unexpected token '{}'", other),
            }
        }
        self.tables.hhea = Some(hhea);
    }

    fn resolve_vhea(&mut self, table: &typed::VheaTable) {
        let mut vhea = tables::vhea::Vhea::default();
        for record in table.metrics() {
            let keyword = record.keyword();
            match keyword.kind {
                Kind::VertTypoAscenderKw => vhea.ascender = record.metric().parse().into(),
                Kind::VertTypoDescenderKw => vhea.descender = record.metric().parse().into(),
                Kind::VertTypoLineGapKw => vhea.line_gap = record.metric().parse().into(),
                other => panic!("bug in parser, unexpected token '{}'", other),
            }
        }
        self.tables.vhea = Some(vhea);
    }

    fn resolve_vmtx(&mut self, table: &typed::VmtxTable) {
        let mut vmtx = super::tables::VmtxBuilder::default();
        for item in table.statements() {
            let glyph = self.resolve_glyph(&item.glyph());
            let value = item.value().parse_signed();
            match item.keyword().kind {
                Kind::VertAdvanceYKw => vmtx.advances_y.push((glyph, value)),
                Kind::VertOriginYKw => vmtx.origins_y.push((glyph, value)),
                _ => unreachable!(),
            }
        }
        self.tables.vmtx = Some(vmtx);
    }

    fn resolve_gdef(&mut self, table: &typed::GdefTable) {
        let mut gdef = super::tables::GdefBuilder::default();
        for statement in table.statements() {
            match statement {
                typed::GdefTableItem::Attach(rule) => {
                    let glyphs = self.resolve_glyph_or_class(&rule.target());
                    let indices = rule
                        .indices()
                        .map(|n| n.parse_unsigned().unwrap())
                        .collect::<Vec<_>>();
                    assert!(!indices.is_empty(), "check this in validation");
                    for glyph in glyphs.iter() {
                        gdef.attach
                            .entry(glyph)
                            .or_default()
                            .extend(indices.iter().copied());
                    }
                }
                typed::GdefTableItem::LigatureCaret(rule) => {
                    let target = rule.target();
                    let glyphs = self.resolve_glyph_or_class(&target);
                    let mut carets: Vec<_> = match rule.values() {
                        typed::LigatureCaretValue::Pos(items) => items
                            .values()
                            .map(|n| CaretValue::format_1(n.parse_signed()))
                            .collect(),
                        typed::LigatureCaretValue::Index(items) => items
                            .values()
                            .map(|n| CaretValue::format_2(n.parse_unsigned().unwrap()))
                            .collect(),
                    };
                    carets.sort_by_key(|c| match c {
                        CaretValue::Format1(table) => table.coordinate as i32,
                        CaretValue::Format2(table) => table.caret_value_point_index as i32,
                        CaretValue::Format3(table) => table.coordinate as i32,
                    });
                    for glyph in glyphs.iter() {
                        gdef.ligature_pos
                            .entry(glyph)
                            .or_insert_with(|| carets.clone());
                    }
                }

                typed::GdefTableItem::ClassDef(rule) => {
                    for (class, id) in [
                        (rule.base_glyphs(), ClassId::Base),
                        (rule.ligature_glyphs(), ClassId::Ligature),
                        (rule.mark_glyphs(), ClassId::Mark),
                        (rule.component_glyphs(), ClassId::Component),
                    ] {
                        let Some(class) = class else { continue; };
                        if let Err((bad_glyph, old_class)) =
                            gdef.add_glyph_class(self.resolve_glyph_class(&class), id)
                        {
                            let bad_glyph_name = self.reverse_glyph_map.get(&bad_glyph).unwrap();
                            self.error(class.range(), format!("class includes glyph '{bad_glyph_name}', already in class {old_class}"));
                        }
                    }
                }
            }
        }
        self.tables.gdef = Some(gdef);
    }

    fn resolve_head(&mut self, table: &typed::HeadTable) {
        let mut head = super::tables::HeadBuilder::default();
        let font_rev = table.statements().last().unwrap().value();
        head.font_revision = font_rev.parse_fixed();
        self.tables.head = Some(head);
    }

    fn resolve_name_spec(&mut self, node: &typed::NameSpec) -> super::tables::NameSpec {
        const WIN_DEFAULT_IDS: (u16, u16) = (1, 0x0409);
        const MAC_DEFAULT_IDS: (u16, u16) = (0, 0);

        let platform_id = node
            .platform_id()
            .map(|n| n.parse().unwrap())
            .unwrap_or(tags::WIN_PLATFORM_ID);

        let (encoding_id, language_id) = match node.platform_and_language_ids() {
            Some((platform, language)) => (platform.parse().unwrap(), language.parse().unwrap()),
            None => match platform_id {
                tags::MAC_PLATFORM_ID => MAC_DEFAULT_IDS,
                tags::WIN_PLATFORM_ID => WIN_DEFAULT_IDS,
                _ => panic!("missed validation"),
            },
        };
        super::tables::NameSpec {
            platform_id,
            encoding_id,
            language_id,
            string: node.string().text.clone(),
        }
    }

    fn resolve_lookup_ref(&mut self, lookup: typed::LookupRef) {
        let id = self
            .lookups
            .get_named(&lookup.label().text)
            .expect("checked in validation pass");
        self.add_lookup_to_current_feature_if_present(id);
    }

    fn resolve_lookup_block(&mut self, lookup: typed::LookupBlock) {
        self.start_lookup_block(lookup.tag());

        //let use_extension = lookup.use_extension().is_some();
        for item in lookup.statements() {
            self.resolve_statement(item);
        }
        self.end_lookup_block();
    }

    fn resolve_statement(&mut self, item: &NodeOrToken) {
        if let Some(script) = typed::Script::cast(item) {
            self.set_script(script);
        } else if let Some(language) = typed::Language::cast(item) {
            self.set_language(language);
        } else if let Some(lookupflag) = typed::LookupFlag::cast(item) {
            self.set_lookup_flag(lookupflag);
        } else if let Some(glyph_def) = typed::GlyphClassDef::cast(item) {
            self.define_glyph_class(glyph_def);
        } else if let Some(glyph_def) = typed::MarkClassDef::cast(item) {
            self.define_mark_class(glyph_def);
        } else if item.kind() == Kind::SubtableNode {
            self.add_subtable_break();
        } else if let Some(lookup) = typed::LookupRef::cast(item) {
            self.resolve_lookup_ref(lookup);
        } else if let Some(lookup) = typed::LookupBlock::cast(item) {
            self.resolve_lookup_block(lookup);
        } else if let Some(rule) = typed::GsubStatement::cast(item) {
            self.add_gsub_statement(rule);
        } else if let Some(rule) = typed::GposStatement::cast(item) {
            self.add_gpos_statement(rule)
        } else {
            let span = match item {
                NodeOrToken::Token(t) => t.range(),
                NodeOrToken::Node(node) => {
                    let range = node.range();
                    let end = range.end.min(range.start + 16);
                    range.start..end
                }
            };
            self.error(span, format!("unhandled statement: '{}'", item.kind()));
        }
    }

    fn define_named_anchor(&mut self, anchor_def: typed::AnchorDef) {
        let anchor_block = anchor_def.anchor();
        let name = anchor_def.name();
        let anchor = match self.resolve_anchor(&anchor_block) {
            Some(a @ AnchorTable::Format1(_) | a @ AnchorTable::Format2(_)) => a,
            Some(_) => {
                return self.error(
                    anchor_block.range(),
                    "named anchor definition can only be in format A or B",
                )
            }
            None => return,
        };
        if let Some(_prev) = self
            .anchor_defs
            .insert(name.text.clone(), (anchor, anchor_def.range().start))
        {
            self.error(name.range(), "duplicate anchor definition");
        }
    }

    fn resolve_anchor(&mut self, item: &typed::Anchor) -> Option<AnchorTable> {
        if let Some((x, y)) = item.coords().map(|(x, y)| (x.parse(), y.parse())) {
            if let Some(point) = item.contourpoint() {
                match point.parse_unsigned() {
                    Some(point) => return Some(AnchorTable::format_2(x, y, point)),
                    None => panic!("negative contourpoint, go fix your parser"),
                }
            } else if let Some((x_coord, y_coord)) = item.devices() {
                return Some(AnchorTable::format_3(
                    x,
                    y,
                    x_coord.compile(),
                    y_coord.compile(),
                ));
            } else {
                return Some(AnchorTable::format_1(x, y));
            }
        } else if let Some(name) = item.name() {
            match self.anchor_defs.get(&name.text) {
                Some((anchor, pos)) if *pos < item.range().start => return Some(anchor.clone()),
                _ => {
                    self.error(name.range(), "anchor is not defined");
                    return None;
                }
            }
        } else if item.null().is_some() {
            return None;
        }
        panic!("bad anchor {:?} go check your parser", item);
    }

    fn resolve_glyph_or_class(&mut self, item: &typed::GlyphOrClass) -> GlyphOrClass {
        match item {
            typed::GlyphOrClass::Glyph(name) => GlyphOrClass::Glyph(self.resolve_glyph_name(name)),
            typed::GlyphOrClass::Cid(cid) => GlyphOrClass::Glyph(self.resolve_cid(cid)),
            typed::GlyphOrClass::Class(class) => {
                GlyphOrClass::Class(self.resolve_glyph_class_literal(class))
            }
            typed::GlyphOrClass::NamedClass(name) => {
                GlyphOrClass::Class(self.resolve_named_glyph_class(name))
            }
            typed::GlyphOrClass::Null(_) => GlyphOrClass::Null,
        }
    }

    fn resolve_glyph(&mut self, item: &typed::Glyph) -> GlyphId {
        match item {
            typed::Glyph::Named(name) => self.resolve_glyph_name(name),
            typed::Glyph::Cid(name) => self.resolve_cid(name),
            typed::Glyph::Null(_) => GlyphId::NOTDEF,
        }
    }

    fn resolve_glyph_class(&mut self, item: &typed::GlyphClass) -> GlyphClass {
        match item {
            typed::GlyphClass::Named(name) => self.resolve_named_glyph_class(name),
            typed::GlyphClass::Literal(lit) => self.resolve_glyph_class_literal(lit),
        }
    }

    fn resolve_glyph_class_literal(&mut self, class: &typed::GlyphClassLiteral) -> GlyphClass {
        let mut glyphs = Vec::new();
        for item in class.items() {
            if let Some(id) =
                typed::GlyphName::cast(item).map(|name| self.resolve_glyph_name(&name))
            {
                glyphs.push(id);
            } else if let Some(id) = typed::Cid::cast(item).map(|cid| self.resolve_cid(&cid)) {
                glyphs.push(id);
            } else if let Some(range) = typed::GlyphRange::cast(item) {
                self.add_glyphs_from_range(&range, &mut glyphs);
            } else if let Some(alias) = typed::GlyphClassName::cast(item) {
                glyphs.extend(self.resolve_named_glyph_class(&alias).items());
            } else {
                panic!("unexptected kind in class literal: '{}'", item.kind());
            }
        }
        glyphs.into()
    }

    fn resolve_named_glyph_class(&mut self, name: &typed::GlyphClassName) -> GlyphClass {
        self.glyph_class_defs
            .get(name.text())
            .cloned()
            .or_else(|| {
                self.mark_classes.get(name.text()).map(|cls| {
                    cls.members
                        .iter()
                        .flat_map(|(glyphs, _)| glyphs.iter())
                        .collect()
                })
            })
            .unwrap()
    }

    fn resolve_glyph_name(&mut self, name: &typed::GlyphName) -> GlyphId {
        self.glyph_map.get(name.text()).unwrap()
    }

    fn resolve_lookahead_sequence(
        &mut self,
        seq: impl Iterator<Item = typed::GlyphOrClass>,
    ) -> Vec<GlyphOrClass> {
        seq.map(|inp| self.resolve_glyph_or_class(&inp)).collect()
    }

    fn resolve_backtrack_sequence(
        &mut self,
        seq: impl Iterator<Item = typed::GlyphOrClass>,
    ) -> Vec<GlyphOrClass> {
        let mut result = self.resolve_lookahead_sequence(seq);
        result.reverse();
        result
    }

    fn resolve_cid(&mut self, cid: &typed::Cid) -> GlyphId {
        self.glyph_map.get(&cid.parse()).unwrap()
    }

    fn add_glyphs_from_range(&mut self, range: &typed::GlyphRange, out: &mut Vec<GlyphId>) {
        let start = range.start();
        let end = range.end();

        match (start.kind, end.kind) {
            (Kind::Cid, Kind::Cid) => {
                if let Err(err) = glyph_range::cid(start, end, |cid| {
                    match self.glyph_map.get(&cid) {
                        Some(id) => out.push(id),
                        None => {
                            // this is techincally allowed, but we error for now
                            self.error(
                                range.range(),
                                format!("Range member '{}' does not exist in font", cid),
                            );
                        }
                    }
                }) {
                    self.error(range.range(), err);
                }
            }
            (Kind::GlyphName, Kind::GlyphName) => {
                if let Err(err) = glyph_range::named(start, end, |name| {
                    match self.glyph_map.get(name) {
                        Some(id) => out.push(id),
                        None => {
                            // this is techincally allowed, but we error for now
                            self.error(
                                range.range(),
                                format!("Range member '{}' does not exist in font", name),
                            );
                        }
                    }
                }) {
                    self.error(range.range(), err);
                }
            }
            (_, _) => self.error(range.range(), "Invalid types in glyph range"),
        }
    }
}

fn sequence_enumerator(sequence: &[GlyphOrClass]) -> Vec<Vec<GlyphId>> {
    assert!(sequence.len() >= 2);
    let split = sequence.split_first();
    let mut result = Vec::new();
    let (left, right) = split.unwrap();
    sequence_enumerator_impl(Vec::new(), left, right, &mut result);
    result
}

fn sequence_enumerator_impl(
    prefix: Vec<GlyphId>,
    left: &GlyphOrClass,
    right: &[GlyphOrClass],
    acc: &mut Vec<Vec<GlyphId>>,
) {
    for glyph in left.iter() {
        let mut prefix = prefix.clone();
        prefix.push(glyph);

        match right.split_first() {
            Some((head, tail)) => sequence_enumerator_impl(prefix, head, tail, acc),
            None => acc.push(prefix),
        }
    }
}

//FIXME: sometimes a glyph class should be unique/sorted and sometimes order matters
//and dupes are allowed?
//fn make_ctx_glyphs(item: &GlyphOrClass) -> BTreeSet<GlyphId> {
//item.iter().collect()
//}

#[cfg(test)]
mod tests {
    use super::*;

    fn glyph_id_vec<const N: usize>(ids: [u16; N]) -> Vec<GlyphId> {
        ids.iter().copied().map(GlyphId::new).collect()
    }

    #[test]
    fn sequence_enumerator_smoke_test() {
        let sequence = vec![
            GlyphOrClass::Glyph(GlyphId::new(1)),
            GlyphOrClass::Class([2_u16, 3, 4].iter().copied().map(GlyphId::new).collect()),
            GlyphOrClass::Class([8, 9].iter().copied().map(GlyphId::new).collect()),
        ];

        assert_eq!(
            sequence_enumerator(&sequence),
            vec![
                glyph_id_vec([1, 2, 8]),
                glyph_id_vec([1, 2, 9]),
                glyph_id_vec([1, 3, 8]),
                glyph_id_vec([1, 3, 9]),
                glyph_id_vec([1, 4, 8]),
                glyph_id_vec([1, 4, 9]),
            ]
        );
    }
}
