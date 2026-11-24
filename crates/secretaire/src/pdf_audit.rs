//! Advisory checks on the PDF and its source mapping after a successful build.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use fuzukue_proto::{PdfAudit, PdfAuditCheck, PdfAuditFinding, PdfAuditStatus};
use hayro_syntax::object::{Array, Dict, MaybeRef, Name, ObjRef, Object, String as PdfString};
use hayro_syntax::xref::XRef;
use hayro_syntax::Pdf;

use PdfAuditStatus::{Fail, Incomplete, Pass};

mod sync;

fn check(status: PdfAuditStatus) -> PdfAuditCheck {
    PdfAuditCheck {
        status,
        findings: Vec::new(),
    }
}

fn add(
    check: &mut PdfAuditCheck,
    status: PdfAuditStatus,
    code: &str,
    location: &str,
    detail: impl Into<String>,
) {
    if check.status == Pass || status == Incomplete {
        check.status = status;
    }
    check.findings.push(PdfAuditFinding {
        code: code.into(),
        location: location.into(),
        detail: detail.into(),
        tool: None,
    });
}

pub(crate) async fn audit(project: &Path, pdf: &str) -> PdfAudit {
    let path = project.join(pdf);
    let output = path.clone();
    let inspection = tokio::task::spawn_blocking(move || inspect_pdf(&output)).await;
    let (links, bookmarks, pages) = match inspection {
        Ok(Ok(inspection)) => inspection,
        Err(error) => unreadable_pdf(pdf, format!("PDF inspection failed: {error}")),
        Ok(Err(error)) => unreadable_pdf(pdf, format!("{error:#}")),
    };
    let synctex = sync::audit(project, &path, pages, Path::new("synctex")).await;
    PdfAudit {
        pdf: pdf.into(),
        links,
        bookmarks,
        synctex,
    }
}

fn unreadable_pdf(pdf: &str, detail: String) -> (PdfAuditCheck, PdfAuditCheck, Option<usize>) {
    let mut result = check(Incomplete);
    add(&mut result, Incomplete, "unreadable_pdf", pdf, detail);
    (result.clone(), result, None)
}

fn inspect_pdf(path: &Path) -> anyhow::Result<(PdfAuditCheck, PdfAuditCheck, Option<usize>)> {
    let pdf = Pdf::new(std::fs::read(path)?)
        .map_err(|error| anyhow::anyhow!("Cannot parse PDF: {error:?}"))?;
    let xref = pdf.xref();
    let catalog = xref
        .get::<Dict>(xref.root_id())
        .ok_or_else(|| anyhow::anyhow!("PDF catalog is missing"))?;

    let mut links = check(Pass);
    let pages = page_tree(xref, &catalog, &mut links);
    let page_count = (links.status == Pass).then_some(pages.len());
    let page_refs = pages.iter().map(|(reference, _)| *reference).collect();
    let named = named_destinations(xref, &catalog, &mut links);
    let destinations = Destinations {
        xref,
        pages: page_refs,
        named,
    };

    for (index, (_, page)) in pages.iter().enumerate() {
        let Some(annots) = page.get::<Array>("Annots") else {
            if page.contains_key("Annots") {
                add(
                    &mut links,
                    Fail,
                    "invalid_annotations",
                    &format!("Page {}", index + 1),
                    "/Annots is not an array",
                );
            }
            continue;
        };
        for (annotation, entry) in annots.raw_iter().enumerate() {
            let location = format!("Page {}, annotation {}", index + 1, annotation + 1);
            match resolve(xref, &entry) {
                Some(Object::Dict(dict)) if has_type(&dict, "Subtype", "Link") => {
                    destinations.check_target(&dict, &location, &mut links);
                }
                Some(Object::Dict(_)) => {}
                _ => add(
                    &mut links,
                    Fail,
                    "invalid_annotation",
                    &location,
                    "Annotation object is missing or invalid",
                ),
            }
        }
    }

    let mut bookmarks = check(Pass);
    if page_count.is_none() {
        add(
            &mut bookmarks,
            Incomplete,
            "invalid_page_tree",
            "/Pages",
            "Could not confirm destination page membership",
        );
    }
    if let Some(root) = catalog.get::<Dict>("Outlines") {
        walk_outline(
            &destinations,
            &root,
            "Outlines",
            &mut HashSet::new(),
            0,
            &mut bookmarks,
        );
    } else if catalog.contains_key("Outlines") {
        add(
            &mut bookmarks,
            Fail,
            "invalid_outline",
            "/Outlines",
            "Outline root is missing or invalid",
        );
    }
    Ok((links, bookmarks, page_count))
}

fn resolve<'a>(xref: &'a XRef, value: &MaybeRef<Object<'a>>) -> Option<Object<'a>> {
    match value {
        MaybeRef::Ref(reference) => xref.get((*reference).into()),
        MaybeRef::NotRef(object) => Some(object.clone()),
    }
}

fn has_type(dict: &Dict<'_>, key: &str, name: &str) -> bool {
    dict.get::<Name>(key).as_deref() == Some(name.as_bytes())
}

// Walk the catalog's tree so that unreferenced /Page objects cannot satisfy destinations.
fn page_tree<'a>(
    xref: &'a XRef,
    catalog: &Dict<'a>,
    result: &mut PdfAuditCheck,
) -> Vec<(ObjRef, Dict<'a>)> {
    let mut pages = Vec::new();
    let Some(root) = catalog.get_ref("Pages") else {
        add(
            result,
            Fail,
            "invalid_page_tree",
            "/Pages",
            "Page tree root is missing or invalid",
        );
        return pages;
    };
    let mut visited = HashSet::new();
    let mut pending = vec![root];
    while let Some(reference) = pending.pop() {
        let location = format!("Page tree object {reference}");
        if !visited.insert(reference) {
            add(
                result,
                Fail,
                "repeated_page_object",
                &location,
                "Page tree contains a cycle or a repeated child",
            );
            continue;
        }
        match xref.get::<Dict>(reference.into()) {
            Some(node) if reference != root && has_type(&node, "Type", "Page") => {
                pages.push((reference, node))
            }
            Some(node) if has_type(&node, "Type", "Pages") => {
                if let Some(kids) = node.get::<Array>("Kids") {
                    let children: Vec<_> = kids
                        .raw_iter()
                        .filter_map(|child| {
                            child.as_obj_ref().or_else(|| {
                                add(
                                    result,
                                    Fail,
                                    "invalid_page_tree",
                                    &location,
                                    "/Kids contains a direct object",
                                );
                                None
                            })
                        })
                        .collect();
                    pending.extend(children.into_iter().rev());
                } else {
                    add(
                        result,
                        Fail,
                        "invalid_page_tree",
                        &location,
                        "/Kids is missing or invalid",
                    );
                }
            }
            _ => add(
                result,
                Fail,
                "invalid_page_tree",
                &location,
                "Expected a /Page or /Pages dictionary",
            ),
        }
    }
    pages
}

#[derive(PartialEq, Eq, Hash)]
enum DestinationName {
    Name(Vec<u8>),
    String(Vec<u8>),
}

type NamedDestinations<'a> = HashMap<DestinationName, MaybeRef<Object<'a>>>;

fn named_destinations<'a>(
    xref: &'a XRef,
    catalog: &Dict<'a>,
    result: &mut PdfAuditCheck,
) -> NamedDestinations<'a> {
    let mut named = HashMap::new();
    if let Some(legacy) = catalog.get::<Dict>("Dests") {
        for name in legacy.keys() {
            if let Some(value) = legacy.get_raw(&name) {
                named.insert(DestinationName::Name(name.to_vec()), value);
            } else {
                add(
                    result,
                    Fail,
                    "invalid_destination",
                    "/Dests",
                    format!("Cannot read destination {name}"),
                );
            }
        }
    } else if catalog.contains_key("Dests") {
        add(
            result,
            Fail,
            "invalid_name_tree",
            "/Dests",
            "Legacy destinations dictionary is invalid",
        );
    }

    let Some(names) = catalog.get::<Dict>("Names") else {
        if catalog.contains_key("Names") {
            add(
                result,
                Fail,
                "invalid_name_tree",
                "/Names",
                "Names dictionary is invalid",
            );
        }
        return named;
    };
    let Some(root) = names.get_raw::<Object>("Dests") else {
        return named;
    };
    let mut visited = HashSet::new();
    let mut pending = vec![root];
    while let Some(entry) = pending.pop() {
        let location = "/Names/Dests";
        if entry.as_obj_ref().is_some_and(|id| !visited.insert(id)) {
            add(
                result,
                Fail,
                "destination_tree_cycle",
                location,
                "Repeated node in the destination name tree",
            );
            continue;
        }
        let Some(Object::Dict(node)) = resolve(xref, &entry) else {
            add(
                result,
                Fail,
                "invalid_name_tree",
                location,
                "Destination name tree node is missing or invalid",
            );
            continue;
        };
        if let Some(kids) = node.get::<Array>("Kids") {
            pending.extend(kids.raw_iter());
        } else if node.contains_key("Kids") {
            add(
                result,
                Fail,
                "invalid_name_tree",
                location,
                "/Kids is not an array",
            );
        }
        if let Some(entries) = node.get::<Array>("Names") {
            let mut pairs = entries.raw_iter();
            while let Some(key) = pairs.next() {
                match (resolve(xref, &key), pairs.next()) {
                    (Some(Object::String(key)), Some(value)) => {
                        let key = key.as_bytes().to_vec();
                        if named
                            .insert(DestinationName::String(key.clone()), value)
                            .is_some()
                        {
                            add(
                                result,
                                Fail,
                                "duplicate_destination",
                                location,
                                format!("Repeated name {:?}", String::from_utf8_lossy(&key)),
                            );
                        }
                    }
                    _ => add(
                        result,
                        Fail,
                        "invalid_name_tree",
                        location,
                        "/Names must contain pairs of strings and destinations",
                    ),
                }
            }
        } else if node.contains_key("Names") {
            add(
                result,
                Fail,
                "invalid_name_tree",
                location,
                "/Names is not an array",
            );
        }
    }
    named
}

struct Destinations<'a> {
    xref: &'a XRef,
    pages: HashSet<ObjRef>,
    named: NamedDestinations<'a>,
}

impl<'a> Destinations<'a> {
    // Returns whether the item declares any target, including external actions.
    fn check_target(&self, item: &Dict<'a>, location: &str, result: &mut PdfAuditCheck) -> bool {
        let has_dest = item.contains_key("Dest");
        let has_action = item.contains_key("A");
        if has_dest {
            self.check_destination(item.get_raw("Dest"), location, result);
        }
        if has_action {
            self.check_actions(item.get_raw("A"), location, result);
        }
        has_dest || has_action
    }

    fn check_actions(
        &self,
        entry: Option<MaybeRef<Object<'a>>>,
        location: &str,
        result: &mut PdfAuditCheck,
    ) {
        // Track the active path because independent branches may share an action.
        enum Visit<'a> {
            Enter {
                entry: Option<MaybeRef<Object<'a>>>,
                allow_list: bool,
            },
            Leave(ObjRef),
        }
        let mut active = HashSet::new();
        let mut finished = HashSet::new();
        let mut pending = vec![Visit::Enter {
            entry,
            allow_list: false,
        }];
        while let Some(visit) = pending.pop() {
            let (entry, allow_list) = match visit {
                Visit::Enter { entry, allow_list } => (entry, allow_list),
                Visit::Leave(id) => {
                    active.remove(&id);
                    finished.insert(id);
                    continue;
                }
            };
            if let Some(id) = entry.as_ref().and_then(MaybeRef::as_obj_ref) {
                if finished.contains(&id) {
                    continue;
                }
                if !active.insert(id) {
                    add(
                        result,
                        Fail,
                        "action_cycle",
                        location,
                        "Action chain refers to itself",
                    );
                    continue;
                }
                pending.push(Visit::Leave(id));
            }
            let action = match entry.as_ref().and_then(|entry| resolve(self.xref, entry)) {
                Some(Object::Dict(action)) => action,
                Some(Object::Array(actions)) if allow_list => {
                    pending.extend(actions.raw_iter().map(|entry| Visit::Enter {
                        entry: Some(entry),
                        allow_list: false,
                    }));
                    continue;
                }
                _ => {
                    add(
                        result,
                        Fail,
                        "invalid_action",
                        location,
                        "Action dictionary is missing or invalid",
                    );
                    continue;
                }
            };
            match action.get::<Name>("S").as_deref() {
                Some(b"GoTo") => self.check_destination(action.get_raw("D"), location, result),
                Some(_) => {}
                None => add(
                    result,
                    Fail,
                    "invalid_action",
                    location,
                    "Action type /S is missing or invalid",
                ),
            }
            if action.contains_key("Next") {
                pending.push(Visit::Enter {
                    entry: action.get_raw("Next"),
                    allow_list: true,
                });
            }
        }
    }

    fn check_destination(
        &self,
        mut dest: Option<MaybeRef<Object<'a>>>,
        location: &str,
        result: &mut PdfAuditCheck,
    ) {
        let mut references = HashSet::new();
        let mut names = HashSet::new();
        loop {
            if dest
                .as_ref()
                .and_then(MaybeRef::as_obj_ref)
                .is_some_and(|id| !references.insert(id))
            {
                break;
            }
            let name = match dest.as_ref().and_then(|entry| resolve(self.xref, entry)) {
                Some(Object::Array(array)) => {
                    self.check_explicit(&array, location, result);
                    return;
                }
                Some(Object::Dict(dict)) => {
                    dest = dict.get_raw("D");
                    continue;
                }
                Some(Object::Name(name)) => DestinationName::Name(name.to_vec()),
                Some(Object::String(name)) => DestinationName::String(name.as_bytes().to_vec()),
                _ => {
                    add(
                        result,
                        Fail,
                        "invalid_destination",
                        location,
                        "Destination is missing or invalid",
                    );
                    return;
                }
            };
            let Some(target) = self.named.get(&name) else {
                let bytes = match name {
                    DestinationName::Name(bytes) | DestinationName::String(bytes) => bytes,
                };
                add(
                    result,
                    Fail,
                    "dangling_destination",
                    location,
                    format!(
                        "Named destination {:?} is undefined",
                        String::from_utf8_lossy(&bytes)
                    ),
                );
                return;
            };
            if !names.insert(name) {
                break;
            }
            dest = Some(match target {
                MaybeRef::Ref(id) => MaybeRef::Ref(*id),
                MaybeRef::NotRef(object) => MaybeRef::NotRef(object.clone()),
            });
        }
        add(
            result,
            Fail,
            "destination_cycle",
            location,
            "Destination refers to itself",
        );
    }

    fn check_explicit(&self, array: &Array<'a>, location: &str, result: &mut PdfAuditCheck) {
        let mut entries = array.raw_iter();
        let Some(page) = entries.next().and_then(|value| value.as_obj_ref()) else {
            add(
                result,
                Fail,
                "invalid_destination",
                location,
                "Expected an indirect page reference",
            );
            return;
        };
        if !self.pages.contains(&page) {
            add(
                result,
                Fail,
                "dangling_destination",
                location,
                format!("Page {page} is outside the document's page tree"),
            );
        }
        let kind = entries.next().and_then(|entry| resolve(self.xref, &entry));
        let parameters = match &kind {
            Some(Object::Name(name)) => match name.as_ref() {
                b"Fit" | b"FitB" => Some(0),
                b"FitH" | b"FitV" | b"FitBH" | b"FitBV" => Some(1),
                b"XYZ" => Some(3),
                b"FitR" => Some(4),
                _ => None,
            },
            _ => None,
        };
        let args: Vec<_> = entries.collect();
        if parameters != Some(args.len())
            || args.iter().any(|arg| {
                !matches!(
                    resolve(self.xref, arg),
                    Some(Object::Number(_) | Object::Null(_))
                )
            })
        {
            add(
                result,
                Fail,
                "invalid_destination",
                location,
                "Destination view or coordinates are invalid",
            );
        }
    }
}

fn walk_outline(
    destinations: &Destinations<'_>,
    owner: &Dict<'_>,
    location: &str,
    visited: &mut HashSet<ObjRef>,
    depth: usize,
    result: &mut PdfAuditCheck,
) {
    let first = owner.get_ref("First");
    if depth >= 128 && first.is_some() {
        add(
            result,
            Incomplete,
            "outline_depth",
            location,
            "Outline is too deeply nested to finish checking",
        );
        return;
    }
    let last = owner.get_ref("Last");
    if owner.contains_key("First") != first.is_some()
        || owner.contains_key("Last") != last.is_some()
        || first.is_some() != last.is_some()
    {
        add(
            result,
            Fail,
            "invalid_outline_hierarchy",
            location,
            "Expected matching /First and /Last child references",
        );
    }
    let (mut current, mut previous) = (first, None);
    while let Some(id) = current {
        let location = format!("Bookmark object {id}");
        if !visited.insert(id) {
            add(
                result,
                Fail,
                "outline_cycle",
                &location,
                "Bookmark is reachable more than once",
            );
            return;
        }
        let Some(item) = destinations.xref.get::<Dict>(id.into()) else {
            add(
                result,
                Fail,
                "dangling_bookmark",
                &location,
                "Bookmark object is missing or invalid",
            );
            return;
        };
        if item.get::<PdfString>("Title").is_none() {
            add(
                result,
                Fail,
                "invalid_bookmark",
                &location,
                "Bookmark title is missing or invalid",
            );
        }
        if item.get_ref("Parent").map(Into::into) != owner.obj_id()
            || item.get_ref("Parent").is_none()
            || item.get_ref("Prev") != previous
            || (previous.is_none() && item.contains_key("Prev"))
        {
            add(
                result,
                Fail,
                "invalid_outline_hierarchy",
                &location,
                "Bookmark /Parent or /Prev does not match the outline tree",
            );
        }
        if !destinations.check_target(&item, &location, result) && !item.contains_key("First") {
            add(
                result,
                Fail,
                "missing_bookmark_target",
                &location,
                "Leaf bookmark has no destination or action",
            );
        }
        walk_outline(destinations, &item, &location, visited, depth + 1, result);
        current = item.get_ref("Next");
        if current.is_none() && item.contains_key("Next") {
            add(
                result,
                Fail,
                "invalid_outline_hierarchy",
                &location,
                "Bookmark /Next is not an indirect reference",
            );
        }
        previous = Some(id);
    }
    if previous != last {
        add(
            result,
            Fail,
            "invalid_outline_hierarchy",
            location,
            "Last child does not match /Last",
        );
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) const VALID_PDF: &[u8] = include_bytes!("../tests/fixtures/pdf-audit/valid.pdf");
    pub(crate) const DEFECTS_PDF: &[u8] = include_bytes!("../tests/fixtures/pdf-audit/defects.pdf");

    #[test]
    fn accepts_explicit_and_named_links_and_nested_bookmarks() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let pdf = dir.path().join("valid.pdf");
        std::fs::write(&pdf, VALID_PDF)?;
        let (links, bookmarks, pages) = inspect_pdf(&pdf)?;
        assert_eq!(pages, Some(1));
        assert_eq!(links, check(Pass));
        assert_eq!(bookmarks, check(Pass));
        Ok(())
    }

    #[test]
    fn reports_dangling_destinations_and_malformed_bookmarks() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let pdf = dir.path().join("defects.pdf");
        std::fs::write(&pdf, DEFECTS_PDF)?;
        let (links, bookmarks, pages) = inspect_pdf(&pdf)?;
        assert_eq!(pages, Some(1));
        assert_eq!(links.status, Fail);
        assert_eq!(bookmarks.status, Fail);
        let has = |check: &PdfAuditCheck, code: &str| {
            check.findings.iter().any(|finding| finding.code == code)
        };
        for code in [
            "destination_tree_cycle",
            "destination_cycle",
            "dangling_destination",
        ] {
            assert!(has(&links, code), "{links:?}");
        }
        assert!(links
            .findings
            .iter()
            .any(|finding| finding.detail.contains("8 0 R is outside")));
        assert!(links
            .findings
            .iter()
            .any(|finding| finding.detail.contains("undefined")));
        for code in [
            "missing_bookmark_target",
            "invalid_bookmark",
            "invalid_outline_hierarchy",
            "outline_cycle",
            "dangling_destination",
        ] {
            assert!(has(&bookmarks, code), "{bookmarks:?}");
        }
        Ok(())
    }
}
