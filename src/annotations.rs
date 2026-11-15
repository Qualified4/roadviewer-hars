//! Bookmarks and tags with the validation order and messages of Python `server.py`.
use crate::{
    http::App,
    parity::{Answer, Fail, TAG_COLORS, nfc, number, other, py_strip, py_words},
    store::id,
};
use axum::{
    Json,
    body::Bytes,
    extract::{Path, State},
    http::{Method, StatusCode},
};
use serde_json::{Map, Value, json};

const BOOKMARK_LIMIT: usize = 8192;
const TAG_LIMIT: usize = 65536;

fn bookmark_body(body: &[u8]) -> Answer<Map<String, Value>> {
    let bad = |m: &str| Fail::bad("invalid_bookmark", m);
    let mut body = match serde_json::from_slice::<Value>(body) {
        Ok(Value::Object(o))
            if o.len() == 3 && ["time", "title", "note"].iter().all(|k| o.contains_key(*k)) =>
        {
            o
        }
        _ => return Err(bad("북마크 요청이 올바르지 않습니다.")),
    };
    if !number(&body["time"]).is_some_and(|t| t.is_finite() && (0.0..=86400.).contains(&t)) {
        return Err(bad("북마크 시간이 올바르지 않습니다."));
    }
    for (key, limit) in [("title", 80), ("note", 1000)] {
        let Some(text) = body[key].as_str() else {
            return Err(bad("제목과 메모는 문자열이어야 합니다."));
        };
        let text = nfc(text);
        let text = py_strip(&text);
        if text.chars().count() > limit || text.chars().any(|c| other(c) && c != '\n' && c != '\t')
        {
            return Err(bad("제목은 80자, 메모는 1000자 이내로 입력해 주세요."));
        }
        body.insert(key.into(), json!(text));
    }
    let title = py_words(body["title"].as_str().unwrap());
    if title.is_empty() {
        return Err(bad("북마크 제목을 입력해 주세요."));
    }
    body.insert("title".into(), json!(title));
    Ok(body)
}

async fn bookmark_update(
    app: &App,
    name: &str,
    bookmark: Option<&str>,
    method: Method,
    body: &[u8],
) -> Answer<(StatusCode, Json<Value>)> {
    if body.len() > BOOKMARK_LIMIT {
        return Err(Fail::too_large());
    }
    let fields = if matches!(method, Method::POST | Method::PUT) {
        Some(bookmark_body(body)?)
    } else {
        None
    };
    let lock = app.store.record_lock(name).await;
    let _lock = lock.lock().await;
    app.store.folder(name).await?;
    let mut meta = app.store.meta(name).await?;
    let mut items = meta["bookmarks"].as_array().cloned().unwrap_or_default();
    if method == Method::GET {
        return Ok((StatusCode::OK, Json(json!({"bookmarks":items}))));
    }
    if bookmark.is_some_and(|id| !items.iter().any(|b| b["id"] == id)) {
        return Err(Fail::not_found());
    }
    items.retain(|b| bookmark.is_none_or(|id| b["id"] != id));
    if let Some(fields) = fields {
        let time = number(&fields["time"]).unwrap();
        if number(&meta["duration"]).is_some_and(|d| d.is_finite() && time > d) {
            return Err(Fail::bad(
                "invalid_bookmark",
                "구간 길이를 벗어난 북마크입니다.",
            ));
        }
        if method == Method::POST && items.len() >= 100 {
            return Err(Fail::bad(
                "invalid_bookmark",
                "북마크는 구간당 최대 100개입니다.",
            ));
        }
        let mut item = Map::new();
        item.insert(
            "id".into(),
            json!(bookmark.map(str::to_owned).unwrap_or_else(id)),
        );
        item.extend(fields);
        items.push(Value::Object(item));
    }
    items.sort_by(|a, b| {
        number(&a["time"])
            .unwrap_or(0.)
            .total_cmp(&number(&b["time"]).unwrap_or(0.))
            .then_with(|| a["id"].as_str().cmp(&b["id"].as_str()))
    });
    meta["bookmarks"] = json!(items);
    app.store.save_meta(meta).await?;
    let status = if method == Method::POST {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(json!({"bookmarks":items}))))
}
pub async fn bookmarks(
    State(app): State<App>,
    Path(id): Path<String>,
    method: Method,
    body: Bytes,
) -> Answer<(StatusCode, Json<Value>)> {
    bookmark_update(&app, &id, None, method, &body).await
}
pub async fn bookmark_item(
    State(app): State<App>,
    Path((id, bookmark)): Path<(String, String)>,
    method: Method,
    body: Bytes,
) -> Answer<(StatusCode, Json<Value>)> {
    bookmark_update(&app, &id, Some(&bookmark), method, &body).await
}

/// A validated tag list in request order; a later duplicate name replaces the earlier one.
fn tag_body(body: &[u8]) -> Answer<(String, Vec<(String, Value)>)> {
    let bad = |m: &str| Fail::bad("invalid_tags", m);
    let body = match serde_json::from_slice::<Value>(body) {
        Ok(Value::Object(o)) => o,
        _ => return Err(bad("태그 요청이 올바르지 않습니다.")),
    };
    let action = body.get("action").map(|v| v.as_str().unwrap_or("\0"));
    let action = action.unwrap_or("set");
    if body.keys().any(|k| k != "action" && k != "tags")
        || !["set", "add", "remove"].contains(&action)
    {
        return Err(bad("태그 요청이 올바르지 않습니다."));
    }
    let Some(values) = body
        .get("tags")
        .and_then(Value::as_array)
        .filter(|v| v.len() <= 100)
    else {
        return Err(bad("태그는 구간당 최대 100개입니다."));
    };
    let mut tags: Vec<(String, Value)> = Vec::new();
    for value in values {
        let Some(tag) = value
            .as_object()
            .filter(|o| o.keys().all(|k| k == "name" || k == "color"))
        else {
            return Err(bad("태그 이름이 올바르지 않습니다."));
        };
        let Some(name) = tag.get("name").and_then(Value::as_str) else {
            return Err(bad("태그 이름이 올바르지 않습니다."));
        };
        let name = py_words(&nfc(name));
        if name.is_empty() || name.chars().count() > 32 || name.chars().any(other) {
            return Err(bad(
                "태그 이름은 1~32자의 표시 가능한 문자로 입력해 주세요.",
            ));
        }
        let color = match tag.get("color") {
            None => "mint",
            Some(v) => match v.as_str() {
                Some(c) if TAG_COLORS.contains(&c) => c,
                _ => return Err(bad("태그 색상이 올바르지 않습니다.")),
            },
        };
        let key = name.to_lowercase();
        let entry = json!({"name":name,"color":color});
        match tags.iter_mut().find(|(k, _)| *k == key) {
            Some(slot) => slot.1 = entry,
            None => tags.push((key, entry)),
        }
    }
    Ok((action.to_owned(), tags))
}
pub async fn tags(
    State(app): State<App>,
    Path(id): Path<String>,
    body: Bytes,
) -> Answer<Json<Value>> {
    if body.len() > TAG_LIMIT {
        return Err(Fail::too_large());
    }
    let (action, tags) = tag_body(&body)?;
    let lock = app.store.record_lock(&id).await;
    let _lock = lock.lock().await;
    app.store.folder(&id).await?;
    let mut meta = app.store.meta(&id).await?;
    let mut current: Vec<(String, Value)> = meta["tags"]
        .as_array()
        .into_iter()
        .flatten()
        .map(|t| (t["name"].as_str().unwrap_or("").to_lowercase(), t.clone()))
        .collect();
    match action.as_str() {
        "add" => {
            for (key, value) in tags {
                if !current.iter().any(|(k, _)| *k == key) {
                    current.push((key, value));
                }
            }
        }
        "remove" => current.retain(|(k, _)| !tags.iter().any(|(t, _)| t == k)),
        _ => current = tags,
    }
    if current.len() > 100 {
        return Err(Fail::bad("invalid_tags", "태그는 구간당 최대 100개입니다."));
    }
    meta["tags"] = json!(current.into_iter().map(|(_, v)| v).collect::<Vec<_>>());
    let response = json!({"tags":meta["tags"]});
    app.store.save_meta(meta).await?;
    Ok(Json(response))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn tag_and_bookmark_text_rules_follow_python() {
        let (_, tags) =
            tag_body(br#"{"tags":[{"name":" a\nb "},{"name":"A B","color":"red"}]}"#).unwrap();
        assert_eq!(tags.len(), 1);
        assert_eq!(tags[0].1, json!({"name":"A B","color":"red"}));
        let joiner = serde_json::to_vec(&json!({"tags":[{"name":"x\u{200d}y"}]})).unwrap();
        assert!(tag_body(&joiner).is_err());
        assert!(tag_body(br#"{"action":null,"tags":[]}"#).is_err());
        let b = bookmark_body(br#"{"time":1,"title":"  a\n\tb ","note":" n\n "}"#).unwrap();
        assert_eq!(
            (b["title"].clone(), b["note"].clone()),
            (json!("a b"), json!("n"))
        );
        assert_eq!(b["time"], json!(1));
        assert!(bookmark_body(br#"{"time":true,"title":"a","note":""}"#).is_err());
        let separator =
            serde_json::to_vec(&json!({"time":1,"title":"\u{2028}","note":""})).unwrap();
        assert!(bookmark_body(&separator).is_err());
    }
}
