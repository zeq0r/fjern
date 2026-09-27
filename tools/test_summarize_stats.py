import unittest

from summarize_stats import parse, summarize


class StatsTests(unittest.TestCase):
    def test_interval_summary_ignores_other_log_lines(self):
        rows = parse("""Connecting
RDP stats: updates/s=10.0 paint-attempts/s=8.0 paint-max-ms=2.0 snapshot-wait-max-ms=3.0 input-queue-max-ms=1.0 rss-mib=40.0
RDP stats: updates/s=20.0 paint-attempts/s=12.0 paint-max-ms=4.0 snapshot-wait-max-ms=5.0 input-queue-max-ms=2.0 rss-mib=42.0
""")
        self.assertEqual(summarize(rows), {
            "protocol": "RDP", "intervals": 2,
            "mean_updates_per_second": 15.0,
            "mean_paint_attempts_per_second": 10.0,
            "peak_rss_mib": 42.0,
            "max_paint_ms": 4.0,
            "max_snapshot_wait_ms": 5.0,
            "max_input_queue_ms": 2.0,
        })

    def test_mixed_or_incomplete_logs_fail(self):
        with self.assertRaisesRegex(ValueError, "mixes"):
            parse("RDP stats: updates/s=1 paint-attempts/s=1 rss-mib=1\n"
                  "VNC stats: updates/s=1 paint-attempts/s=1 rss-mib=1")
        with self.assertRaisesRegex(ValueError, "incomplete"):
            parse("VNC stats: updates/s=1")

    def test_rdp_handoff_counts_are_summarized(self):
        rows = parse(
            "RDP stats: updates/s=100 published/s=60 replaced/s=4 "
            "published-row-changes/s=58 picked/s=56 paint-attempts/s=55 "
            "paint-new/s=54 paint-row-changes/s=51 rss-mib=40"
        )
        report = summarize(rows)
        self.assertEqual(report["mean_published_per_second"], 60)
        self.assertEqual(report["mean_replaced_per_second"], 4)
        self.assertEqual(report["mean_published_row_changes_per_second"], 58)
        self.assertEqual(report["mean_picked_per_second"], 56)
        self.assertEqual(report["mean_paint_new_per_second"], 54)
        self.assertEqual(report["mean_paint_row_changes_per_second"], 51)

    def test_vnc_event_max_is_reported_separately(self):
        rows = parse("VNC stats: updates/s=59.3 paint-attempts/s=3.5 "
                     "event-max-ms=2.2 scale-ms=40.0 raw-pending=false rss-mib=42.0")
        report = summarize(rows)
        self.assertEqual(report["max_event_batch_ms"], 2.2)
        self.assertNotIn("max_snapshot_wait_ms", report)


if __name__ == "__main__":
    unittest.main()
