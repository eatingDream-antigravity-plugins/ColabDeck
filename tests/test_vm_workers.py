import importlib.util
import os
from pathlib import Path
import tempfile
import unittest
from contextlib import ExitStack
from unittest.mock import MagicMock, Mock, mock_open, patch
from urllib.error import HTTPError


def load_asset(relative_path):
    file = Path(__file__).resolve().parents[1] / "assets" / relative_path
    spec = importlib.util.spec_from_file_location("worker_fixture", file)
    module = importlib.util.module_from_spec(spec)
    with patch.dict(os.environ, {}, clear=True):
        spec.loader.exec_module(module)
    return module


class WorkerSafety(unittest.TestCase):
    def test_invalid_api_key_fails_before_hardware_or_downloads(self):
        worker = load_asset("plugins/llama-cpp/worker.py")
        with (
            patch.object(worker, "REPO", "fixture/repo"),
            patch.object(worker, "API_KEY", "invalid\nkey"),
            patch.object(worker.signal, "signal"),
            patch.object(worker, "detect_gpu") as gpu,
            patch.object(worker, "pick_gguf_files") as metadata,
            patch.object(worker, "download_model") as download,
            patch.object(worker, "write_status") as status,
        ):
            self.assertEqual(worker.main(), 1)
            gpu.assert_not_called()
            metadata.assert_not_called()
            download.assert_not_called()
            self.assertEqual(status.call_args.args[:2], ("failed", "detect"))

    def test_failed_cuda_warmup_never_reports_ready(self):
        worker = load_asset("plugins/llama-cpp/worker.py")
        health = MagicMock()
        health.__enter__.return_value.status = 200
        warmup = HTTPError("http://fixture.invalid", 503, "fixture", None, None)
        with tempfile.TemporaryDirectory() as directory, ExitStack() as cleanup:
            settings = {
                "REPO": "fixture/repo",
                "RUN_DIR": directory,
                "detect_gpu": Mock(return_value=("Fixture GPU", 16000)),
                "pick_gguf_files": Mock(return_value={"quant": "Q4", "files": []}),
                "ensure_llama_server": Mock(return_value="fixture-server"),
                "download_model": Mock(return_value="fixture.gguf"),
                "serve_until_exit": Mock(),
                "write_status": Mock(),
                "HTTP": Mock(open=Mock(side_effect=[health, warmup])),
            }
            for name, replacement in settings.items():
                cleanup.enter_context(patch.object(worker, name, replacement))
            cleanup.enter_context(patch.object(worker.signal, "signal"))
            cleanup.enter_context(patch.object(worker.os, "umask"))
            cleanup.enter_context(patch.object(worker.socket, "socket"))
            process = cleanup.enter_context(patch.object(worker.subprocess, "Popen"))
            process.return_value.poll.return_value = None
            self.assertEqual(worker.main(), 1)
            calls = [call.args for call in worker.write_status.call_args_list]
            self.assertFalse(any(call[0] == "ready" for call in calls))
            self.assertEqual(calls[-1][:2], ("failed", "warmup"))
            self.assertIn("HTTP 503", calls[-1][3])
            worker.serve_until_exit.assert_not_called()
            process.return_value.terminate.assert_called_once()

    def test_watchdog_rechecks_cancellation_before_release(self):
        script = load_asset("watchdog.py")
        watchdog = script.Watchdog("fixture.json", "fixture")
        watchdog.release_url = "http://fixture.invalid/unassign"
        watchdog.config = {"instance": "fixture", "cancelled": True}
        with patch.object(watchdog, "load"), patch.object(watchdog, "http") as http:
            self.assertFalse(watchdog.release("deadline", lambda: True))
            http.open.assert_not_called()

    def test_watchdog_resumes_when_candidate_activation_rolls_back(self):
        script = load_asset("watchdog.py")
        watchdog = script.Watchdog("fixture.json", "original")
        watchdog.release_url = "http://fixture.invalid/unassign"
        original = {"instance": "original", "deadline_epoch": 1}
        policies = iter([original, {"instance": "candidate"}, original])

        def load():
            watchdog.config = next(policies)

        with (
            patch.object(watchdog, "load", side_effect=load),
            patch.object(watchdog, "log"),
            patch("builtins.open", mock_open()),
            patch.object(script.fcntl, "flock") as lock,
            patch.object(watchdog, "release", return_value=True) as release,
        ):
            self.assertEqual(watchdog.run(), 0)
            lock.assert_called_once()
            self.assertEqual(lock.call_args.args[1], script.fcntl.LOCK_SH)
            self.assertEqual(release.call_args.args[0], "deadline")
