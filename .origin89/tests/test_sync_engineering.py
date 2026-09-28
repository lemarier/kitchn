import importlib.util
import io
import json
from pathlib import Path
import tarfile
import tempfile
import unittest
from unittest import mock
import urllib.error

SCRIPT = Path(__file__).resolve().parents[1] / 'sync-engineering.py'
spec = importlib.util.spec_from_file_location('sync_engineering', SCRIPT)
sync = importlib.util.module_from_spec(spec)
spec.loader.exec_module(sync)

BASE = ('origin89-working', 'origin89-writing', 'origin89-commits')
A, B = 'a' * 40, 'b' * 40


def archive(names=BASE, suffix='', extra=()):
    output = io.BytesIO()
    with tarfile.open(fileobj=output, mode='w:gz') as bundle:
        for name in names:
            data = f'---\nname: {name}\ndescription: Example task guidance.\n---\nDo the work. {suffix}\n'.encode()
            info = tarfile.TarInfo(f'engineering-main/skills/{name}/SKILL.md')
            info.size = len(data)
            bundle.addfile(info, io.BytesIO(data))
        for path, data, kind in extra:
            info = tarfile.TarInfo(path)
            info.type = kind
            info.size = len(data)
            if kind == tarfile.SYMTYPE:
                info.linkname = '/tmp/outside'
            bundle.addfile(info, io.BytesIO(data))
    return output.getvalue()


class Remote:
    def __init__(self, revision=A, data=None):
        self.revision = revision
        self.data = archive() if data is None else data
        self.calls = []

    def __call__(self, url):
        self.calls.append(url)
        if url == sync.HEAD_URL:
            return json.dumps({'sha': self.revision}).encode()
        return self.data


def unavailable(url):
    raise urllib.error.URLError('offline')


class RefreshTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.project = Path(self.temp.name)
        self.cache = self.project / '.origin89/engineering'
        self.discovery = self.project / '.agents/skills'
        self.claude = self.project / '.claude/skills'

    def install(self, remote=None):
        return sync.refresh(self.project, remote or Remote())

    def assert_current(self, revision):
        self.assertEqual((self.cache / 'current').readlink().as_posix(), f'versions/{revision}')
        self.assertFalse((self.cache / 'sync.lock').exists())

    def test_first_install_creates_discoverable_skills_and_revision(self):
        remote = Remote()
        state = self.install(remote)
        self.assertEqual(state['revision'], A)
        self.assertIsNone(state['cached'])
        self.assertEqual(len(remote.calls), 2)
        for name in BASE:
            self.assertTrue((self.discovery / name).is_symlink())
            self.assertIn('Do the work.', (self.discovery / name / 'SKILL.md').read_text())
            self.assertTrue((self.claude / name).is_symlink())
            self.assertEqual((self.discovery / name).resolve(), (self.claude / name).resolve())
        self.assert_current(A)


    def test_unchanged_revision_uses_one_request(self):
        self.install()
        remote = Remote()
        self.install(remote)
        self.assertEqual(remote.calls, [sync.HEAD_URL])
        self.assert_current(A)

    def test_update_retains_previous_task_snapshot(self):
        old = self.install()
        self.install(Remote(B, archive(suffix='Updated.')))
        self.assertNotIn('Updated.', (Path(old['path']) / 'skills/origin89-working/SKILL.md').read_text())
        self.assertIn('Updated.', (self.discovery / 'origin89-working/SKILL.md').read_text())
        self.assertIn('Updated.', (self.claude / 'origin89-working/SKILL.md').read_text())
        self.assert_current(B)

    def test_removed_upstream_skill_removes_only_managed_link(self):
        self.install(Remote(data=archive(BASE + ('origin89-rust',))))
        local = self.discovery / 'board-specific'
        local.mkdir()
        (local / 'SKILL.md').write_text('Local rules')
        self.install(Remote(B))
        self.assertFalse((self.discovery / 'origin89-rust').is_symlink())
        self.assertFalse((self.claude / 'origin89-rust').is_symlink())
        self.assertEqual((local / 'SKILL.md').read_text(), 'Local rules')
        self.assert_current(B)

    def test_network_failure_reports_verified_cache(self):
        self.install()
        state = sync.refresh(self.project, unavailable)
        self.assertIn('offline', state['cached'])
        self.assertEqual(state['revision'], A)
        self.assert_current(A)

    def test_explicit_offline_never_fetches(self):
        self.install()
        state = sync.refresh(self.project, lambda _: self.fail('network used'), offline=True)
        self.assertEqual(state['cached'], 'offline requested')

    def test_existing_cache_gains_claude_discovery_offline(self):
        self.install()
        for link in self.claude.iterdir():
            link.unlink()
        self.claude.rmdir()
        self.claude.parent.rmdir()
        sync.refresh(self.project, lambda _: self.fail('network used'), offline=True)
        for name in BASE:
            self.assertEqual((self.claude / name).resolve(), (self.discovery / name).resolve())
        self.assert_current(A)

    def test_claude_conflict_keeps_snapshot_and_both_discovery_sets(self):
        self.install()
        local = self.claude / 'origin89-rust'
        local.mkdir()
        (local / 'SKILL.md').write_text('Local rules')
        with self.assertRaisesRegex(ValueError, 'local skill files'):
            self.install(Remote(B, archive(BASE + ('origin89-rust',))))
        self.assertFalse((self.discovery / 'origin89-rust').exists())
        self.assertEqual((local / 'SKILL.md').read_text(), 'Local rules')
        self.assert_current(A)

    def test_failed_activation_preserves_discovery_and_can_retry(self):
        for installed in (False, True):
            for failure in ('claude', 'next', 'current'):
                with self.subTest(installed=installed, failure=failure), tempfile.TemporaryDirectory() as scratch:
                    project = Path(scratch).resolve()
                    cache = project / '.origin89/engineering'
                    if installed:
                        sync.refresh(project, Remote())
                    discoveries = [project / assistant / 'skills' for assistant in ('.agents', '.claude')]
                    for discovery in discoveries:
                        local = discovery / 'board-specific'
                        local.mkdir(parents=True)
                        (local / 'SKILL.md').write_text('Local rules')
                    before = [{p.name: p.readlink() for p in d.iterdir() if p.is_symlink()}
                              for d in discoveries]
                    symlink_to, replace = Path.symlink_to, sync.os.replace

                    def fail_symlink(path, target, **kwargs):
                        if ((failure == 'claude' and path == discoveries[1] / 'origin89-rust')
                                or (failure == 'next' and path == cache / 'next')):
                            raise PermissionError('injected activation failure')
                        return symlink_to(path, target, **kwargs)

                    def fail_replace(source, destination):
                        if failure == 'current' and destination == cache / 'current':
                            raise PermissionError('injected activation failure')
                        return replace(source, destination)

                    remote = Remote(B, archive(BASE + ('origin89-rust',), suffix='Updated.'))
                    with mock.patch.object(Path, 'symlink_to', fail_symlink), mock.patch.object(sync.os, 'replace', fail_replace):
                        with self.assertRaisesRegex(PermissionError, 'injected activation failure'):
                            sync.refresh(project, remote)
                    for discovery, original in zip(discoveries, before):
                        self.assertEqual({p.name: p.readlink() for p in discovery.iterdir() if p.is_symlink()}, original)
                        self.assertEqual((discovery / 'board-specific/SKILL.md').read_text(), 'Local rules')
                        for name in original:
                            self.assertNotIn('Updated.', (discovery / name / 'SKILL.md').read_text())
                    if installed:
                        self.assertEqual((cache / 'current').readlink().as_posix(), f'versions/{A}')
                    else:
                        self.assertFalse((cache / 'current').is_symlink())
                    self.assertFalse((cache / 'next').is_symlink())
                    self.assertFalse((cache / 'sync.lock').exists())
                    sync.refresh(project, remote)
                    self.assertEqual((cache / 'current').readlink().as_posix(), f'versions/{B}')
                    for discovery in discoveries:
                        self.assertIn('Updated.', (discovery / 'origin89-rust/SKILL.md').read_text())

    def test_claude_custom_link_is_not_replaced(self):
        self.claude.mkdir(parents=True)
        link = self.claude / BASE[0]
        link.symlink_to('../../../custom')
        with self.assertRaisesRegex(ValueError, 'custom skill link'):
            self.install()
        self.assertEqual(str(link.readlink()), '../../../custom')
        self.assertFalse((self.discovery / BASE[0]).is_symlink())

    def test_claude_local_skills_survive_update_and_removal(self):
        self.install(Remote(data=archive(BASE + ('origin89-rust',))))
        local = self.claude / 'board-specific'
        local.mkdir()
        (local / 'SKILL.md').write_text('Local rules')
        self.install(Remote(B))
        self.assertFalse((self.claude / 'origin89-rust').is_symlink())
        self.assertEqual((local / 'SKILL.md').read_text(), 'Local rules')
        self.assert_current(B)

    def test_redirected_claude_discovery_is_rejected_before_activation(self):
        for location in ('.claude', '.claude/skills'):
            with self.subTest(location=location), tempfile.TemporaryDirectory() as scratch:
                project = Path(scratch)
                link = project / location
                link.parent.mkdir(exist_ok=True)
                outside = project / 'outside'
                outside.mkdir()
                link.symlink_to(outside, target_is_directory=True)
                with self.assertRaisesRegex(ValueError, 'normal directory'):
                    sync.refresh(project, Remote())
                self.assertEqual(list(outside.iterdir()), [])
                self.assertFalse((project / '.agents/skills' / BASE[0]).is_symlink())

    def test_offline_first_use_fails_cleanly(self):
        with self.assertRaisesRegex(ValueError, 'No cached'):
            sync.refresh(self.project, unavailable)
        self.assertFalse((self.cache / 'sync.lock').exists())

    def test_invalid_metadata_keeps_existing_revision(self):
        self.install()
        for revision in (None, 'main', '../outside', 123):
            with self.subTest(revision=revision), self.assertRaisesRegex(ValueError, 'valid engineering revision'):
                self.install(Remote(revision))
            self.assert_current(A)

    def test_invalid_archive_keeps_existing_revision(self):
        self.install()
        with self.assertRaises(tarfile.TarError):
            self.install(Remote(B, b'invalid gzip'))
        self.assert_current(A)

    def test_custom_folder_conflict_can_be_resolved_and_retried(self):
        self.discovery.mkdir(parents=True)
        local = self.discovery / BASE[0]
        local.mkdir()
        (local / 'SKILL.md').write_text('Preserve me')
        with self.assertRaisesRegex(ValueError, 'local skill files'):
            self.install()
        self.assertEqual((local / 'SKILL.md').read_text(), 'Preserve me')
        local.rename(self.discovery / 'project-working')
        self.install()
        self.assert_current(A)

    def test_custom_symlink_is_not_replaced(self):
        self.discovery.mkdir(parents=True)
        link = self.discovery / BASE[0]
        link.symlink_to('../../../custom')
        with self.assertRaisesRegex(ValueError, 'custom skill link'):
            self.install()
        self.assertEqual(str(link.readlink()), '../../../custom')

    def test_modified_cache_is_not_silently_overwritten(self):
        self.install()
        (self.discovery / BASE[0] / 'SKILL.md').write_text('Local modification')
        with self.assertRaisesRegex(ValueError, 'modified'):
            self.install(Remote(B))
        self.assertFalse((self.cache / 'sync.lock').exists())

    def test_nested_state_json_is_verified(self):
        data = archive(extra=[('repo/skills/origin89-working/state.json', b'{}', tarfile.REGTYPE)])
        self.install(Remote(data=data))
        self.install(Remote(data=data))
        (self.discovery / BASE[0] / 'state.json').write_text('tampered')
        with self.assertRaisesRegex(ValueError, 'modified'):
            self.install()

    def test_redirected_cache_is_rejected_without_writing_target(self):
        outside = self.project / 'outside'
        outside.mkdir()
        (self.project / '.origin89').symlink_to(outside, target_is_directory=True)
        with self.assertRaisesRegex(ValueError, 'normal directory'):
            self.install()
        self.assertEqual(list(outside.iterdir()), [])

    def test_redirected_discovery_is_rejected(self):
        (self.project / '.agents').symlink_to(self.project, target_is_directory=True)
        with self.assertRaisesRegex(ValueError, 'normal directory'):
            self.install()

    def test_stale_lock_fails_without_waiting(self):
        self.cache.mkdir(parents=True)
        (self.cache / 'sync.lock').mkdir()
        with self.assertRaisesRegex(ValueError, 'Another refresh'):
            self.install()
        self.assertTrue((self.cache / 'sync.lock').exists())

    def test_pending_pointer_keeps_previous_snapshot(self):
        self.install()
        (self.cache / 'next').symlink_to('unexpected')
        with self.assertRaisesRegex(ValueError, 'pending cache pointer'):
            self.install(Remote(B))
        self.assert_current(A)

    def test_download_failure_after_metadata_preserves_cache(self):
        self.install()
        def fetch(url):
            return json.dumps({'sha': B}).encode() if url == sync.HEAD_URL else unavailable(url)
        result = sync.refresh(self.project, fetch)
        self.assertIn('offline', result['cached'])
        self.assert_current(A)


class ArchiveTests(unittest.TestCase):
    def test_unsafe_or_duplicate_entries_rejected(self):
        cases = [
            ('repo/skills/origin89-working/../../outside', b'x', tarfile.REGTYPE),
            ('repo/skills/origin89-working/link', b'', tarfile.SYMTYPE),
            ('repo/skills/origin89-working/SKILL.md', b'x', tarfile.REGTYPE),
            ('repo/skills/origin89-working/large', b'x' * (256 * 1024 + 1), tarfile.REGTYPE),
            ('repo/skills/custom/SKILL.md', b'x', tarfile.REGTYPE),
        ]
        for entry in cases:
            with self.subTest(path=entry[0]), self.assertRaises(ValueError):
                sync.skill_files(archive(extra=[entry]))

    def test_missing_required_skill_rejected(self):
        with self.assertRaisesRegex(ValueError, 'missing'):
            sync.skill_files(archive(BASE[:2]))

    def test_invalid_frontmatter_rejected(self):
        with self.assertRaisesRegex(ValueError, 'frontmatter'):
            sync.skill_files(archive(extra=[('repo/skills/origin89-rust/SKILL.md', b'No header', tarfile.REGTYPE)]))

    def test_total_archive_limit_applies_to_non_skill_content(self):
        with self.assertRaisesRegex(ValueError, 'size or entry limit'):
            sync.skill_files(archive(extra=[('repo/large', b'x' * (sync.MAX_CONTENT + 1), tarfile.REGTYPE)]))


if __name__ == '__main__':
    unittest.main()
