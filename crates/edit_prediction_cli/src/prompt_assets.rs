use std::borrow::Cow;

#[cfg(feature = "dynamic_prompts")]
pub fn get_prompt(name: &'static str) -> Cow<'static, str> {
    use anyhow::Context;
    use std::collections::HashMap;
    use std::path::Path;
    use std::sync::{LazyLock, RwLock};

    const PROMPTS_DIR: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/src/prompts");

    static PROMPT_CACHE: LazyLock<RwLock<HashMap<&'static str, &'static str>>> =
        LazyLock::new(|| RwLock::new(HashMap::default()));

    let filesystem_path = Path::new(PROMPTS_DIR).join(name);
    if let Some(cached_contents) = PROMPT_CACHE.read().unwrap().get(name) {
        return Cow::Borrowed(cached_contents);
    }
    let contents = std::fs::read_to_string(&filesystem_path)
        .context(name)
        .expect("Failed to read prompt");
    let leaked = contents.leak();
    PROMPT_CACHE.write().unwrap().insert(name, leaked);
    return Cow::Borrowed(leaked);
}

#[cfg(not(feature = "dynamic_prompts"))]
pub fn get_prompt(name: &'static str) -> Cow<'static, str> {
    use include_dir::{include_dir, Dir};

    static PROMPTS: Dir<'static> = include_dir!("$CARGO_MANIFEST_DIR/src/prompts");

    match PROMPTS.get_file(name) {
        Some(file) => {
            let content = String::from_utf8_lossy(file.contents());
            Cow::Owned(content.into_owned())
        }
        None => panic!("prompt file not found: {name}"),
    }
}
