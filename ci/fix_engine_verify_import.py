from pathlib import Path

path = Path("bridge-core/src/engine.rs")
text = path.read_text(encoding="utf-8")
old = '''    publish_vnext_incremental_generation, recommend_system_build_tuning,
    verify_built_index_for_generation_adoption, verify_generation_structure,
    verify_positional2_sidecars, verify_positional3_sidecars, verify_positional_sidecars,
'''
new = '''    publish_vnext_incremental_generation, recommend_system_build_tuning,
    verify_built_index_for_generation_adoption, verify_generation, verify_generation_structure,
    verify_positional2_sidecars, verify_positional3_sidecars, verify_positional_sidecars,
'''
count = text.count(old)
if count != 1:
    raise SystemExit(f"engine verify import block: expected 1 match, got {count}")
path.write_text(text.replace(old, new, 1), encoding="utf-8")
print("engine full verify import restored")
