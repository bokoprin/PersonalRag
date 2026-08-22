from pathlib import Path

path = Path("ci/apply_verified_generation_fast_path.py")
text = path.read_text(encoding="utf-8")

old_import = '''engine = replace_once(
    engine,
    "initialize_generation_from_built_index",
    "initialize_generation_from_verified_built_index",
    "engine adoption import",
)
'''
new_import = '''engine = replace_once(
    engine,
    "    initialize_generation_from_built_index, initialize_vnext_generation_store,\\n",
    "    initialize_generation_from_verified_built_index, initialize_vnext_generation_store,\\n",
    "engine adoption import",
)
'''
if text.count(old_import) != 1:
    raise SystemExit(f"import selector block mismatch: {text.count(old_import)}")
text = text.replace(old_import, new_import, 1)

old_call = '''    "        initialize_generation_from_verified_built_index(build_dir, &base_index_path, &identities)\\n            .map_err(|error| error.to_string())?;",
'''
new_call = '''    "        initialize_generation_from_built_index(build_dir, &base_index_path, &identities)\\n            .map_err(|error| error.to_string())?;",
'''
if text.count(old_call) != 1:
    raise SystemExit(f"call selector block mismatch: {text.count(old_call)}")
text = text.replace(old_call, new_call, 1)

path.write_text(text, encoding="utf-8")
print("verified generation transform selectors fixed")
