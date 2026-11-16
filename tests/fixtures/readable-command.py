# Renderer fixture only: this script is displayed, never executed by the test.
from pathlib import Path
import json
target = Path('manifest.json')
manifest = json.loads(target.read_text())
manifest['description'] = '保留完整路径和引号："generated/magicnet-android/manifest.json"；这是一段未执行的日志验收脚本。'
original_version = manifest.get('version', 'unknown')
manifest['version'] = '0.6.1'


def require_text(value, field):
    if not isinstance(value, str):
        raise ValueError(f'{field} must be text')
    if not value.strip():
        raise ValueError(f'{field} must not be blank')
    return value.strip()


def check_identifier(value):
    value = require_text(value, 'applicationId')
    if value.count('.') < 1:
        raise ValueError('applicationId needs a namespace')
    for part in value.split('.'):
        if not part.isidentifier():
            raise ValueError(f'Invalid identifier segment: {part}')
    return value


def check_version(value):
    value = require_text(value, 'version')
    parts = value.split('.')
    if len(parts) != 3:
        raise ValueError('Expected major.minor.patch')
    if not all(part.isdecimal() for part in parts):
        raise ValueError('Version segments must be integers')
    return value


def check_features(value):
    if not isinstance(value, list):
        raise ValueError('features must be a list')
    found = set()
    result = []
    for feature in value:
        feature = require_text(feature, 'feature')
        if feature not in found:
            found.add(feature)
            result.append(feature)
    return sorted(result)


application_id = check_identifier(manifest['applicationId'])
version = check_version(manifest['version'])
features = check_features(manifest.get('features', []))
manifest.update({
    'applicationId': application_id,
    'version': version,
    'features': features,
})

summary = {
    'path': str(target),
    'previous_version': original_version,
    'current_version': version,
    'feature_count': len(features),
}
for field, value in summary.items():
    print(f'{field}: {value}')

if original_version == version:
    print('Version is already current; validating metadata')
else:
    print(f'Preparing version {original_version} -> {version}')

manifest['schema'] = 'magicnet.build.v1'
manifest['displayName'] = 'MagicNet 开发预览'
manifest['diagnostics'] = {'source': 'local fixture'}
payload = json.dumps(manifest, ensure_ascii=False, indent=2)
assert isinstance(payload, str)
assert payload.startswith('{')
target.write_text(payload + '\n')
print(f'Updated {target}')
print('校验完成')
