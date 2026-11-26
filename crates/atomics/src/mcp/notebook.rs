use super::*;
use atomics_core::notebook::{Edit, Request};

pub(super) fn options() -> Value {
    let Ok(capability) = std::env::var(crate::notebook_auth::ENV) else { return json!({"mode":"off"}) };
    // sbx discovers tools before publishing the session. This launch hint only controls discovery;
    // every operation still checks the live capability and current mode in the host.
    let mode = std::env::var(crate::notebook_auth::MODE_ENV).unwrap_or_else(|_| "off".into());
    crate::ipc::request(&ControlRequest::Coordination { capability, request: Request::Options })
        .ok()
        .filter(|r| r.ok)
        .map(|r| r.result)
        .unwrap_or_else(|| json!({"mode":mode}))
}

/// Mark a pull as delivered only after its framed response has reached MCP stdout.
pub(super) fn acknowledge_read(reply: &Value) {
    let Some(text) = reply["result"]["content"][0]["text"].as_str() else { return };
    let Ok(data) = serde_json::from_str::<Value>(text) else { return };
    let Ok(ids) = serde_json::from_value::<Vec<u64>>(data["coordination"]["delivery_ids"].clone()) else { return };
    let Ok(capability) = std::env::var(crate::notebook_auth::ENV) else { return };
    for ids in ids.chunks(atomics_core::notebook::MAX_DELIVERY) {
        let _ = crate::ipc::request(&ControlRequest::Coordination {
            capability: capability.clone(),
            request: Request::Ack { ids: ids.to_vec() },
        });
    }
}

pub(super) fn request(name: &str, args: &Value, capability: Option<String>) -> Option<Result<ControlRequest, String>> {
    let addressed = name == "atomics_note" && (args.get("to").is_some() || args.get("reply_to").is_some());
    let managed = name == "atomics_plan_notebook" && ["hidden", "muted", "task"].iter().any(|k| args.get(k).is_some());
    let deleting = name == "atomics_plan_notebook" && args["delete"] == true && capability.is_some();
    let reading = capability.is_some()
        && (name == "atomics_notes" || (name == "atomics_plan_notebook" && args.get("id").is_none() && !managed));
    if !addressed
        && !managed
        && !deleting
        && !reading
        && ![
            "atomics_rules",
            "atomics_plan_rules",
            "atomics_plan_broadcast",
            "atomics_relay_approve",
            "atomics_note_delivery",
        ]
        .contains(&name)
    {
        return None;
    }
    Some((|| {
        let capability = capability.ok_or("Restart this session to obtain its notebook capability.")?;
        let allowed: &[&str] = match name {
            "atomics_note" => &["text", "to", "reply_to"],
            "atomics_note_delivery" => &["event", "main", "ack"],
            "atomics_relay_approve" => &["plan", "id", "decision", "note"],
            "atomics_notes" | "atomics_rules" => &[],
            "atomics_plan_rules" | "atomics_plan_broadcast" => &["plan", "text"],
            "atomics_plan_notebook" => &["plan", "id", "hidden", "muted", "task", "delete", "after", "compact"],
            _ => return Err("Unknown notebook tool.".into()),
        };
        let map = args.as_object().ok_or("Arguments must be an object.")?;
        if let Some(key) = map.keys().find(|k| !allowed.contains(&k.as_str())) {
            return Err(format!("Unexpected notebook argument {key}."));
        }
        let text = |key: &str| args[key].as_str().map(str::to_owned).ok_or_else(|| format!("{key} must be a string"));
        let request = match name {
            "atomics_note_delivery" => {
                if let Some(ids) = args.get("ack") {
                    if args.get("event").is_some() || args.get("main").is_some() {
                        return Err("Read or acknowledge a delivery, not both.".into());
                    }
                    Request::Ack {
                        ids: serde_json::from_value(ids.clone()).map_err(|_| "Invalid acknowledgement IDs")?,
                    }
                } else {
                    Request::Drain {
                        event: text("event")?,
                        main: args["main"].as_bool().ok_or("main must be boolean")?,
                    }
                }
            }
            "atomics_note" => Request::Send {
                text: text("text")?,
                to: args.get("to").map(|_| text("to")).transpose()?,
                reply_to: args
                    .get("reply_to")
                    .map(|id| id.as_u64().ok_or("reply_to must be an integer"))
                    .transpose()?,
            },
            "atomics_relay_approve" => Request::Manage {
                plan: text("plan")?,
                edit: Edit::Relay {
                    id: args["id"].as_u64().ok_or("id must be positive")?,
                    decision: serde_json::from_value(args["decision"].clone())
                        .map_err(|_| "decision must be approve or deny")?,
                    note: if args.get("note").is_some() { Some(text("note")?) } else { None },
                },
            },
            "atomics_notes" => Request::Read,
            "atomics_rules" => Request::Rules { plan: None, text: None },
            "atomics_plan_rules" => Request::Rules {
                plan: Some(text("plan")?),
                text: if args.get("text").is_some() { Some(text("text")?) } else { None },
            },
            "atomics_plan_broadcast" => Request::Broadcast { plan: text("plan")?, text: text("text")? },
            "atomics_plan_notebook" => {
                let plan = text("plan")?;
                let id = || args["id"].as_u64().ok_or("id must be a positive integer".to_string());
                let edit = if let Some(hidden) = args.get("hidden") {
                    if args.get("muted").is_some() || args.get("task").is_some() || args.get("delete").is_some() {
                        return Err("Choose hide, delete or mute.".into());
                    }
                    Some(Edit::Hide { id: id()?, hidden: hidden.as_bool().ok_or("hidden must be boolean")? })
                } else if let Some(muted) = args.get("muted") {
                    if args.get("id").is_some() || args.get("delete").is_some() {
                        return Err("Choose hide, delete or mute.".into());
                    }
                    Some(Edit::Mute {
                        task: if args.get("task").is_some() { Some(text("task")?) } else { None },
                        muted: muted.as_bool().ok_or("muted must be boolean")?,
                    })
                } else if args["delete"] == true {
                    Some(Edit::Delete { id: id()? })
                } else {
                    if map.keys().any(|k| !["plan", "after", "compact"].contains(&k.as_str())) {
                        return Err("Read with plan (and after or compact), or choose hide, delete or mute.".into());
                    }
                    None
                };
                if edit.is_some() && (args.get("after").is_some() || args.get("compact").is_some()) {
                    return Err("after and compact read the notebook; leave them out of an edit.".into());
                }
                match edit {
                    Some(edit) => Request::Manage { plan, edit },
                    None => Request::Inspect {
                        plan,
                        after: args
                            .get("after")
                            .map(|after| after.as_u64().ok_or("after must be an entry id"))
                            .transpose()?,
                        compact: args
                            .get("compact")
                            .map(|c| c.as_bool().ok_or("compact must be boolean"))
                            .transpose()?
                            == Some(true),
                    },
                }
            }
            _ => unreachable!(),
        };
        Ok(ControlRequest::Coordination { capability, request })
    })())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn coordination_tool_identity_is_bound_and_unknown_fields_are_rejected() {
        let args = json!({"text":"fact","to":"b"});
        assert!(
            matches!(request("atomics_note",&args,Some("host-token".into())).unwrap().unwrap(),ControlRequest::Coordination{capability,request:Request::Send{..}} if capability=="host-token")
        );
        assert!(request("atomics_note", &args, None).unwrap().is_err());
        for key in ["pane", "session", "plan", "sender", "at", "capability", "chief"] {
            let mut args = args.clone();
            args[key] = json!("forged");
            assert!(request("atomics_note", &args, Some("bound".into())).unwrap().is_err(), "{key}");
        }
        assert!(request("atomics_note", &json!({"text":"legacy"}), None).is_none());
        assert!(request("atomics_notes", &json!({}), None).is_none());
        assert!(
            request(
                "atomics_plan_notebook",
                &json!({"plan":"p","hidden":true,"muted":true,"id":1}),
                Some("bound".into())
            )
            .unwrap()
            .is_err()
        );
    }
}
