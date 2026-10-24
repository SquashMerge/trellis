//! A physical schema-14 cache is disposable, not migrated in place.
//! Fixture DDL is CACHE_SCHEMA_V14 from 9e9d0fdf:src/store.rs.
#![cfg(feature = "test-support")]

const SCHEMA_14: &str = r#"
CREATE TABLE index_metadata(singleton INTEGER PRIMARY KEY CHECK(singleton=1), schema_version INTEGER NOT NULL CHECK(schema_version=14), extractor_version TEXT NOT NULL CHECK(extractor_version='native-v6-delta-v1'), root_spelling TEXT NOT NULL, root_device TEXT NOT NULL, root_inode TEXT NOT NULL, index_generation TEXT NOT NULL, index_revision INTEGER NOT NULL CHECK(index_revision BETWEEN 0 AND 9007199254740991), last_opened_at INTEGER NOT NULL CHECK(last_opened_at BETWEEN 0 AND 9007199254740991), indexed_at TEXT NOT NULL, stats TEXT NOT NULL, diagnostics TEXT NOT NULL, reconcile_options TEXT);
CREATE TABLE native_producers(id TEXT NOT NULL,version TEXT NOT NULL,executable_hash TEXT NOT NULL CHECK(length(executable_hash)=64),kind TEXT NOT NULL CHECK(kind='native'),position_encoding TEXT NOT NULL CHECK(position_encoding='utf8'),PRIMARY KEY(id,version));
CREATE TABLE native_producer_languages(producer_id TEXT NOT NULL,producer_version TEXT NOT NULL,language TEXT NOT NULL,inventory_authenticated INTEGER NOT NULL CHECK(inventory_authenticated IN (0,1)),ordinal INTEGER NOT NULL CHECK(ordinal>=0),PRIMARY KEY(producer_id,producer_version,language),UNIQUE(producer_id,producer_version,ordinal),FOREIGN KEY(producer_id,producer_version) REFERENCES native_producers(id,version) DEFERRABLE INITIALLY DEFERRED);
CREATE TABLE native_producer_inputs(producer_id TEXT NOT NULL,producer_version TEXT NOT NULL,language TEXT NOT NULL,component_name TEXT NOT NULL,ordinal INTEGER NOT NULL CHECK(ordinal>=0),PRIMARY KEY(producer_id,producer_version,language,component_name),UNIQUE(producer_id,producer_version,language,ordinal),FOREIGN KEY(producer_id,producer_version,language) REFERENCES native_producer_languages(producer_id,producer_version,language) DEFERRABLE INITIALLY DEFERRED);
CREATE TABLE native_source_sets(id TEXT PRIMARY KEY,root_id TEXT NOT NULL);
CREATE TABLE native_source_set_languages(source_set_id TEXT NOT NULL REFERENCES native_source_sets(id) DEFERRABLE INITIALLY DEFERRED,language TEXT NOT NULL,ordinal INTEGER NOT NULL CHECK(ordinal>=0),PRIMARY KEY(source_set_id,language),UNIQUE(source_set_id,ordinal));
CREATE TABLE native_source_set_dependencies(source_set_id TEXT NOT NULL REFERENCES native_source_sets(id) DEFERRABLE INITIALLY DEFERRED,dependency_id TEXT NOT NULL REFERENCES native_source_sets(id) DEFERRABLE INITIALLY DEFERRED,ordinal INTEGER NOT NULL CHECK(ordinal>=0),PRIMARY KEY(source_set_id,dependency_id),UNIQUE(source_set_id,ordinal));
CREATE TABLE native_revision_supersessions(revision_id TEXT PRIMARY KEY REFERENCES native_revisions(id),superseded_at INTEGER NOT NULL CHECK(superseded_at BETWEEN 0 AND 9007199254740991),state TEXT NOT NULL DEFAULT 'retained' CHECK(state IN ('retained','pending','released')));
CREATE INDEX native_supersessions_clock ON native_revision_supersessions(superseded_at,revision_id);
CREATE INDEX native_supersessions_due ON native_revision_supersessions(superseded_at,revision_id) WHERE state='retained';
CREATE TABLE native_revision_release_debt(revision_id TEXT PRIMARY KEY REFERENCES native_revisions(id) DEFERRABLE INITIALLY DEFERRED,phase TEXT NOT NULL CHECK(phase='pending'));
CREATE TABLE native_release_candidate_versions(revision_id TEXT NOT NULL REFERENCES native_revisions(id) DEFERRABLE INITIALLY DEFERRED,vid INTEGER NOT NULL REFERENCES document_versions(vid) DEFERRABLE INITIALLY DEFERRED,PRIMARY KEY(revision_id,vid));
CREATE TABLE native_release_candidate_graphs(revision_id TEXT NOT NULL REFERENCES native_revisions(id) DEFERRABLE INITIALLY DEFERRED,gid INTEGER NOT NULL REFERENCES graph_projections(gid) DEFERRABLE INITIALLY DEFERRED,PRIMARY KEY(revision_id,gid));
CREATE TABLE native_release_candidate_classes(revision_id TEXT NOT NULL REFERENCES native_revisions(id) DEFERRABLE INITIALLY DEFERRED,cid INTEGER NOT NULL REFERENCES class_projections(cid) DEFERRABLE INITIALLY DEFERRED,PRIMARY KEY(revision_id,cid));
CREATE TABLE native_revisions(id TEXT PRIMARY KEY,source_set_id TEXT NOT NULL REFERENCES native_source_sets(id) DEFERRABLE INITIALLY DEFERRED,toolchain_hash TEXT NOT NULL CHECK(length(toolchain_hash)=64),config_hash TEXT NOT NULL CHECK(length(config_hash)=64),dependency_hash TEXT NOT NULL CHECK(length(dependency_hash)=64),native_revision_id TEXT NOT NULL,source_inventory TEXT NOT NULL,dependency_observations TEXT NOT NULL,reconcile_options TEXT,class_warnings TEXT NOT NULL,class_truncated INTEGER NOT NULL CHECK(class_truncated IN (0,1)),graph_stats TEXT NOT NULL CHECK(json_valid(graph_stats) AND json_type(graph_stats)='object'),graph_diagnostics TEXT NOT NULL CHECK(json_valid(graph_diagnostics) AND json_type(graph_diagnostics)='array'),published_index_revision INTEGER NOT NULL CHECK(published_index_revision BETWEEN 0 AND 9007199254740991));
CREATE UNIQUE INDEX native_revisions_pin ON native_revisions(published_index_revision);
CREATE INDEX native_revisions_header_cover ON native_revisions(source_set_id,published_index_revision,id);
CREATE TABLE revision_capture_inputs(revision_id TEXT NOT NULL REFERENCES native_revisions(id) DEFERRABLE INITIALLY DEFERRED,input_key TEXT NOT NULL,payload TEXT NOT NULL,PRIMARY KEY(revision_id,input_key));
CREATE TABLE document_versions(vid INTEGER PRIMARY KEY,id TEXT NOT NULL UNIQUE,language TEXT NOT NULL,path TEXT NOT NULL,content_hash TEXT NOT NULL CHECK(length(content_hash)=64),extraction_context TEXT NOT NULL CHECK(length(extraction_context)=64),producer_id TEXT NOT NULL,producer_version TEXT NOT NULL,byte_length INTEGER NOT NULL CHECK(byte_length>=0 AND byte_length=length(source_bytes)),source_bytes BLOB NOT NULL,native_witness TEXT NOT NULL CHECK(length(native_witness)=64),UNIQUE(language,path,content_hash,extraction_context,producer_id,producer_version),UNIQUE(vid,language,path),UNIQUE(vid,language),FOREIGN KEY(producer_id,producer_version,language) REFERENCES native_producer_languages(producer_id,producer_version,language) DEFERRABLE INITIALLY DEFERRED);
CREATE TABLE graph_projections(gid INTEGER PRIMARY KEY,vid INTEGER NOT NULL,language TEXT NOT NULL,graph_hash TEXT NOT NULL UNIQUE CHECK(length(graph_hash)=64),state TEXT NOT NULL CHECK(state IN ('staged','ready','unavailable')),class_extraction_state TEXT NOT NULL CHECK(class_extraction_state IN ('notApplicable','ready')),class_extraction_payload TEXT,CHECK((class_extraction_state='ready')=(class_extraction_payload IS NOT NULL)),CHECK((language IN ('java','python'))=(class_extraction_state IN ('ready'))),UNIQUE(vid,graph_hash),UNIQUE(gid,vid),FOREIGN KEY(vid,language) REFERENCES document_versions(vid,language) DEFERRABLE INITIALLY DEFERRED);
CREATE TABLE graph_nodes(gid INTEGER NOT NULL REFERENCES graph_projections(gid) DEFERRABLE INITIALLY DEFERRED,id BLOB NOT NULL,name TEXT NOT NULL,kind TEXT NOT NULL,PRIMARY KEY(gid,id));
CREATE TABLE graph_display_labels(gid INTEGER NOT NULL,id BLOB NOT NULL,label TEXT NOT NULL,PRIMARY KEY(gid,id),FOREIGN KEY(gid,id) REFERENCES graph_nodes(gid,id) DEFERRABLE INITIALLY DEFERRED);
CREATE VIRTUAL TABLE graph_node_trigrams USING fts5(name,content='',columnsize=0,tokenize='trigram case_sensitive 1');
CREATE TRIGGER graph_node_trigrams_insert AFTER INSERT ON graph_nodes BEGIN INSERT INTO graph_node_trigrams(rowid,name) VALUES(new.rowid,lower(new.name)); END;
CREATE TRIGGER graph_node_trigrams_delete AFTER DELETE ON graph_nodes BEGIN INSERT INTO graph_node_trigrams(graph_node_trigrams,rowid,name) VALUES('delete',old.rowid,lower(old.name)); END;
CREATE TRIGGER graph_node_trigrams_update AFTER UPDATE OF name ON graph_nodes BEGIN INSERT INTO graph_node_trigrams(graph_node_trigrams,rowid,name) VALUES('delete',old.rowid,lower(old.name)); INSERT INTO graph_node_trigrams(rowid,name) VALUES(new.rowid,lower(new.name)); END;
CREATE TABLE class_projections(cid INTEGER PRIMARY KEY,gid INTEGER NOT NULL REFERENCES graph_projections(gid) DEFERRABLE INITIALLY DEFERRED,content_hash TEXT NOT NULL UNIQUE CHECK(length(content_hash)=64),state TEXT NOT NULL CHECK(state IN ('ready','unavailable')),UNIQUE(gid,content_hash),UNIQUE(cid,gid));
CREATE TABLE classes(cid INTEGER NOT NULL,gid INTEGER NOT NULL,id BLOB NOT NULL,name TEXT NOT NULL,qualified_name TEXT NOT NULL,path TEXT NOT NULL,payload TEXT NOT NULL,PRIMARY KEY(cid,id),FOREIGN KEY(cid,gid) REFERENCES class_projections(cid,gid) DEFERRABLE INITIALLY DEFERRED,FOREIGN KEY(gid,id) REFERENCES graph_nodes(gid,id) DEFERRABLE INITIALLY DEFERRED);
CREATE TABLE class_relations(cid INTEGER NOT NULL,id TEXT NOT NULL,owner BLOB NOT NULL,target BLOB,payload TEXT NOT NULL,PRIMARY KEY(cid,id),FOREIGN KEY(cid,owner) REFERENCES classes(cid,id) DEFERRABLE INITIALLY DEFERRED,FOREIGN KEY(cid,target) REFERENCES classes(cid,id) DEFERRABLE INITIALLY DEFERRED);
CREATE INDEX class_relations_owner ON class_relations(cid,owner,id);
CREATE INDEX class_relations_target ON class_relations(cid,target,id);
CREATE TABLE revision_documents(revision_id TEXT NOT NULL REFERENCES native_revisions(id) DEFERRABLE INITIALLY DEFERRED,language TEXT NOT NULL,path TEXT NOT NULL,vid INTEGER NOT NULL,gid INTEGER NOT NULL,cid INTEGER,capture_stat TEXT,coverage_requested INTEGER NOT NULL CHECK(coverage_requested IN (0,1)),coverage_selected INTEGER NOT NULL CHECK(coverage_selected IN (0,1)),coverage_state TEXT NOT NULL CHECK(coverage_state IN ('notRequested','omitted','unsupported','failed','partial','complete')),coverage_diagnostic TEXT,ordinal INTEGER NOT NULL CHECK(ordinal>=0),CHECK((coverage_state='complete')=(coverage_diagnostic IS NULL)),PRIMARY KEY(revision_id,language,path),UNIQUE(revision_id,ordinal),FOREIGN KEY(vid,language,path) REFERENCES document_versions(vid,language,path) DEFERRABLE INITIALLY DEFERRED,FOREIGN KEY(gid,vid) REFERENCES graph_projections(gid,vid) DEFERRABLE INITIALLY DEFERRED,FOREIGN KEY(cid,gid) REFERENCES class_projections(cid,gid) DEFERRABLE INITIALLY DEFERRED);
CREATE INDEX revision_documents_version ON revision_documents(vid,revision_id);
CREATE INDEX revision_documents_graph ON revision_documents(gid,revision_id);
CREATE INDEX revision_documents_class ON revision_documents(cid,revision_id);
CREATE INDEX revision_documents_path ON revision_documents(revision_id,path);
CREATE TABLE native_version_coverage_roles(vid INTEGER NOT NULL REFERENCES document_versions(vid) DEFERRABLE INITIALLY DEFERRED,role_kind TEXT NOT NULL CHECK(role_kind IN ('supported','observed')),role TEXT NOT NULL CHECK(role IN ('definition','call')),ordinal INTEGER NOT NULL CHECK(ordinal>=0),PRIMARY KEY(vid,role_kind,ordinal),UNIQUE(vid,role_kind,role));
CREATE TABLE native_version_declarations(vid INTEGER NOT NULL REFERENCES document_versions(vid) DEFERRABLE INITIALLY DEFERRED,syntax_id BLOB NOT NULL,owner_syntax_id BLOB,kind TEXT NOT NULL,role TEXT NOT NULL CHECK(role IN ('test','production')),name TEXT,lookup_key TEXT,key_signature_present INTEGER NOT NULL CHECK(key_signature_present IN (0,1)),key_type_parameter_count INTEGER,key_variadic INTEGER,key_ordinal INTEGER NOT NULL CHECK(key_ordinal>=0),start_byte INTEGER NOT NULL,end_byte INTEGER NOT NULL,name_start INTEGER,name_end INTEGER,CHECK(start_byte>=0 AND end_byte>=start_byte),CHECK((name IS NULL)=(lookup_key IS NULL) AND (name IS NULL)=(name_start IS NULL) AND (name_start IS NULL)=(name_end IS NULL)),CHECK(name_start IS NULL OR (name_start>=start_byte AND name_end<=end_byte AND name_end>name_start)),CHECK((key_signature_present=0 AND key_type_parameter_count IS NULL AND key_variadic IS NULL) OR (key_signature_present=1 AND key_type_parameter_count>=0 AND key_variadic IN (0,1))),PRIMARY KEY(vid,syntax_id),FOREIGN KEY(vid,owner_syntax_id) REFERENCES native_version_declarations(vid,syntax_id) DEFERRABLE INITIALLY DEFERRED);
CREATE INDEX native_version_declarations_lookup ON native_version_declarations(lookup_key,vid,syntax_id);
CREATE INDEX native_version_declarations_owner ON native_version_declarations(vid,owner_syntax_id);
CREATE VIRTUAL TABLE native_declaration_trigrams USING fts5(name,content='',columnsize=0,tokenize='trigram case_sensitive 1');
CREATE TRIGGER native_declaration_trigrams_insert AFTER INSERT ON native_version_declarations BEGIN INSERT INTO native_declaration_trigrams(rowid,name) VALUES(new.rowid,lower(new.name)); END;
CREATE TRIGGER native_declaration_trigrams_delete AFTER DELETE ON native_version_declarations BEGIN INSERT INTO native_declaration_trigrams(native_declaration_trigrams,rowid,name) VALUES('delete',old.rowid,lower(old.name)); END;
CREATE TRIGGER native_declaration_trigrams_update AFTER UPDATE OF name ON native_version_declarations BEGIN INSERT INTO native_declaration_trigrams(native_declaration_trigrams,rowid,name) VALUES('delete',old.rowid,lower(old.name)); INSERT INTO native_declaration_trigrams(rowid,name) VALUES(new.rowid,lower(new.name)); END;
CREATE TABLE native_version_declaration_ancestors(vid INTEGER NOT NULL,syntax_id BLOB NOT NULL,ordinal INTEGER NOT NULL CHECK(ordinal>=0),kind TEXT NOT NULL,name TEXT,sibling_ordinal INTEGER NOT NULL CHECK(sibling_ordinal>=0),signature_present INTEGER NOT NULL CHECK(signature_present IN (0,1)),type_parameter_count INTEGER,variadic INTEGER,CHECK((signature_present=0 AND type_parameter_count IS NULL AND variadic IS NULL) OR (signature_present=1 AND type_parameter_count>=0 AND variadic IN (0,1))),PRIMARY KEY(vid,syntax_id,ordinal),FOREIGN KEY(vid,syntax_id) REFERENCES native_version_declarations(vid,syntax_id) DEFERRABLE INITIALLY DEFERRED);
CREATE TABLE native_version_own_signature_types(vid INTEGER NOT NULL,syntax_id BLOB NOT NULL,ordinal INTEGER NOT NULL CHECK(ordinal>=0),type_name TEXT NOT NULL,PRIMARY KEY(vid,syntax_id,ordinal),FOREIGN KEY(vid,syntax_id) REFERENCES native_version_declarations(vid,syntax_id) DEFERRABLE INITIALLY DEFERRED);
CREATE TABLE native_version_ancestor_signature_types(vid INTEGER NOT NULL,syntax_id BLOB NOT NULL,ancestor_ordinal INTEGER NOT NULL CHECK(ancestor_ordinal>=0),ordinal INTEGER NOT NULL CHECK(ordinal>=0),type_name TEXT NOT NULL,PRIMARY KEY(vid,syntax_id,ancestor_ordinal,ordinal),FOREIGN KEY(vid,syntax_id,ancestor_ordinal) REFERENCES native_version_declaration_ancestors(vid,syntax_id,ordinal) DEFERRABLE INITIALLY DEFERRED);
CREATE TABLE native_version_headers(vid INTEGER NOT NULL,syntax_id BLOB NOT NULL,kind TEXT NOT NULL,name TEXT,result_type TEXT,PRIMARY KEY(vid,syntax_id),FOREIGN KEY(vid,syntax_id) REFERENCES native_version_declarations(vid,syntax_id) DEFERRABLE INITIALLY DEFERRED);
CREATE TABLE native_version_header_items(vid INTEGER NOT NULL,syntax_id BLOB NOT NULL,item_kind TEXT NOT NULL CHECK(item_kind IN ('modifier','typeParameter','base')),ordinal INTEGER NOT NULL CHECK(ordinal>=0),value TEXT NOT NULL,PRIMARY KEY(vid,syntax_id,item_kind,ordinal),FOREIGN KEY(vid,syntax_id) REFERENCES native_version_headers(vid,syntax_id) DEFERRABLE INITIALLY DEFERRED);
CREATE TABLE native_version_parameters(vid INTEGER NOT NULL,syntax_id BLOB NOT NULL,ordinal INTEGER NOT NULL CHECK(ordinal>=0),name TEXT,type_name TEXT,variadic INTEGER NOT NULL CHECK(variadic IN (0,1)),PRIMARY KEY(vid,syntax_id,ordinal),FOREIGN KEY(vid,syntax_id) REFERENCES native_version_headers(vid,syntax_id) DEFERRABLE INITIALLY DEFERRED);
CREATE TABLE native_version_calls(vid INTEGER NOT NULL,id BLOB NOT NULL,owner_syntax_id BLOB NOT NULL,ordinal INTEGER NOT NULL CHECK(ordinal>=0),start_byte INTEGER NOT NULL,end_byte INTEGER NOT NULL,callee_start INTEGER,callee_end INTEGER,spelling TEXT,language TEXT NOT NULL,callee_lookup_key TEXT,CHECK(start_byte>=0 AND end_byte>start_byte),CHECK((callee_start IS NULL)=(callee_end IS NULL)),CHECK(callee_start IS NULL OR (callee_start>=start_byte AND callee_end<=end_byte AND callee_end>callee_start)),PRIMARY KEY(vid,id),UNIQUE(vid,owner_syntax_id,ordinal),FOREIGN KEY(vid,owner_syntax_id) REFERENCES native_version_declarations(vid,syntax_id) DEFERRABLE INITIALLY DEFERRED);
CREATE INDEX native_version_calls_incoming ON native_version_calls(language,callee_lookup_key);
CREATE TABLE native_version_control_regions(vid INTEGER NOT NULL,id BLOB NOT NULL,owner_syntax_id BLOB NOT NULL,ordinal INTEGER NOT NULL CHECK(ordinal>=0),kind TEXT NOT NULL,start_byte INTEGER NOT NULL,end_byte INTEGER NOT NULL,parent_id BLOB,arm TEXT,CHECK(start_byte>=0 AND end_byte>start_byte),PRIMARY KEY(vid,id),UNIQUE(vid,owner_syntax_id,ordinal),UNIQUE(vid,id,owner_syntax_id),FOREIGN KEY(vid,owner_syntax_id) REFERENCES native_version_declarations(vid,syntax_id) DEFERRABLE INITIALLY DEFERRED,FOREIGN KEY(vid,parent_id,owner_syntax_id) REFERENCES native_version_control_regions(vid,id,owner_syntax_id) DEFERRABLE INITIALLY DEFERRED);
CREATE INDEX native_version_regions_parent ON native_version_control_regions(vid,parent_id,owner_syntax_id);
CREATE TABLE native_version_call_regions(vid INTEGER NOT NULL,call_id BLOB NOT NULL,region_id BLOB NOT NULL,ordinal INTEGER NOT NULL CHECK(ordinal>=0),PRIMARY KEY(vid,call_id,ordinal),UNIQUE(vid,call_id,region_id),FOREIGN KEY(vid,call_id) REFERENCES native_version_calls(vid,id) DEFERRABLE INITIALLY DEFERRED,FOREIGN KEY(vid,region_id) REFERENCES native_version_control_regions(vid,id) DEFERRABLE INITIALLY DEFERRED);
CREATE TABLE native_binding_epoch(singleton INTEGER PRIMARY KEY CHECK(singleton=1),index_generation TEXT NOT NULL,first_revision INTEGER NOT NULL CHECK(first_revision BETWEEN 1 AND 9007199254740991));
CREATE TABLE revision_producer_bindings(revision_id TEXT PRIMARY KEY REFERENCES native_revisions(id) DEFERRABLE INITIALLY DEFERRED,producer_sha TEXT NOT NULL CHECK(length(producer_sha)=64),binding_sha TEXT NOT NULL CHECK(length(binding_sha)=64));
"#;

mod common;

use std::{
    fs,
    os::unix::fs::{MetadataExt, PermissionsExt},
    sync::{Arc, atomic::AtomicBool},
};
use trellis::{
    index_coordinator::reconcile_workspace,
    indexer::IndexOptions,
    store::{Store, topology::WorkspaceIdentity},
};

#[test]
fn schema14_marker_recreates_exclusively_with_spooled_revision() {
    let (state, roots) = common::fixture();
    let root = state.path().join("work");
    fs::create_dir(&root).unwrap();
    fs::write(root.join("hello.js"), "function hello() { return 1; }\n").unwrap();
    let identity = WorkspaceIdentity::discover(Some(&root), &root).unwrap();
    // Start with a real durable request queue. Recreate must preserve its
    // accepted rows; a missing queue is not a virgin startup after this point.
    let initial = Store::open_for_tests(state.path(), &root).unwrap();
    let queued = initial
        .enqueue_request(&IndexOptions::new(root.clone()), None)
        .unwrap();
    drop(initial);
    let path = roots.index_db(&identity);
    fs::remove_file(&path).unwrap();
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch(SCHEMA_14).unwrap();
    db.pragma_update(None, "user_version", 14_i64).unwrap();
    db.execute(
        "INSERT INTO index_metadata VALUES(1,14,'native-v6-delta-v1',?1,?2,?3,?4,0,0,'','{}','[]',NULL)",
        rusqlite::params![
            identity.root.to_str().unwrap(),
            identity.device.to_string(),
            identity.inode.to_string(),
            uuid::Uuid::new_v4().to_string()
        ],
    ).unwrap();
    assert_eq!(
        db.pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        14
    );
    assert_eq!(
        db.query_row("SELECT schema_version FROM index_metadata", [], |r| r
            .get::<_, i64>(0))
            .unwrap(),
        14
    );
    drop(db);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let old_inode = fs::metadata(&path).unwrap().ino();
    let old_bytes = fs::read(&path).unwrap();
    let report = roots.gc_report_at(1_800_000_000).unwrap();
    assert_eq!(
        (report.derived[0].status, report.derived[0].reason),
        ("eligible", "old_schema")
    );
    assert_eq!(
        fs::read(&path).unwrap(),
        old_bytes,
        "a GC report must not mutate an obsolete index"
    );

    let store = Store::open_for_tests(state.path(), &root).unwrap();
    assert_eq!(
        fs::metadata(&path).unwrap().ino(),
        old_inode,
        "open defers recreation until exclusive publication"
    );
    assert!(
        store
            .status()
            .unwrap_err()
            .to_string()
            .contains("recovery_required")
    );
    let (pin, _) = reconcile_workspace(
        &store,
        &IndexOptions::new(root.clone()),
        &Arc::new(AtomicBool::new(false)),
        |_| {},
    )
    .unwrap();
    assert_eq!(pin.index_revision, 1);
    let preserved = store.request_by_id(&queued.id).unwrap().unwrap();
    assert_eq!(preserved.seq, queued.seq);
    assert_eq!(preserved.state, "queued");
    assert_ne!(fs::metadata(&path).unwrap().ino(), old_inode);
    let rebuilt = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        rebuilt
            .pragma_query_value(None, "user_version", |r| r.get::<_, i64>(0))
            .unwrap(),
        19
    );
    assert_eq!(
        rebuilt
            .query_row("SELECT schema_version FROM index_metadata", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        19
    );
    assert_eq!(
        rebuilt
            .query_row("SELECT index_revision FROM index_metadata", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
}
