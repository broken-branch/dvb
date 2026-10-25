//! Generated owner routes and resource tags stay in sync with the serving router.
use dvb_engine::api;

#[test]
fn openapi_json_is_current() {
    assert!(
        include_str!("../../../contracts/openapi.json") == api::openapi_json().unwrap(),
        "contracts/openapi.json is stale: run scripts/gen-contracts.sh"
    );
}

#[test]
fn each_operation_has_exactly_one_allowed_resource_tag_matching_its_path() {
    let allowed = [
        "version",
        "status",
        "projects",
        "goals",
        "tasks",
        "questions",
        "runs",
        "roles",
        "hires",
        "settings",
        "usage",
        "update",
        "report",
        "logins",
        "github",
        "models",
        "only",
        "events",
    ];
    let doc = serde_json::to_value(api::openapi()).unwrap();
    for (path, item) in doc["paths"].as_object().unwrap() {
        let resource = path.split('/').nth(1).unwrap();
        for (method, operation) in item.as_object().unwrap() {
            let tags = operation["tags"].as_array().unwrap();
            assert_eq!(tags.len(), 1, "{method} {path}: {tags:?}");
            let tag = tags[0].as_str().unwrap();
            assert!(allowed.contains(&tag), "{method} {path}: {tag}");
            assert_eq!(tag, resource, "{method} {path}");
            for status in ["404", "409", "500"] {
                assert!(
                    operation["responses"].get(status).is_some(),
                    "{method} {path}: {status}"
                );
            }
        }
    }
    assert_eq!(
        doc["paths"]["/github/checks"]["get"]["parameters"][0]["in"],
        "query"
    );
    assert_eq!(
        doc["paths"]["/update"]["get"]["responses"]["502"]["description"],
        "Release check failed"
    );
    assert_eq!(
        doc["paths"]["/only"]["put"]["responses"]["409"]["description"],
        "Invalid models or hours"
    );
}
