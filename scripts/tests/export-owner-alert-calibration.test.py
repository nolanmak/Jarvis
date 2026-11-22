#!/usr/bin/env python3
import importlib.util
import io
import json
from pathlib import Path
import sqlite3
import tempfile
import unittest

spec = importlib.util.spec_from_file_location('exporter', Path(__file__).parents[1] / 'export-owner-alert-calibration.py')
exporter = importlib.util.module_from_spec(spec)
spec.loader.exec_module(exporter)

class CalibrationExportTests(unittest.TestCase):
    def test_read_only_deduplicated_cohort_has_no_subjects_and_filters_window(self):
        with tempfile.TemporaryDirectory() as d:
            root = Path(d)
            db = root / 'db'
            wiki = root / 'wiki'
            (wiki / 'people').mkdir(parents=True)
            (wiki / 'people' / 'matt.md').write_text('---\nidentities:\n  email: matt@example.test\nrelationship: close-contact\n---\n')
            c = sqlite3.connect(db)
            c.executescript('''CREATE TABLE owner_alert_priorities(sender,urgency);
                CREATE TABLE emails(messageId PRIMARY KEY,fromEmail,body,subject,platform);
                CREATE TABLE owner_alert_assessments(message_id PRIMARY KEY,assessed_at_ms);
                CREATE TABLE owner_alerts(id PRIMARY KEY,urgency);
                CREATE TABLE triage_calibration_inputs(message_id,observed_at_ms);''')
            c.execute('INSERT INTO owner_alert_priorities VALUES (?,?)',('person:matt','high'))
            for id, t in [('one',99),('two',98),('old',1)]:
                c.execute('INSERT INTO emails VALUES (?,?,?,?,?)',(id,'matt@example.test','can you send the deck?','PRIVATE SUBJECT','gmail'))
                c.execute('INSERT INTO owner_alert_assessments VALUES (?,?)',(id,t))
                c.execute('INSERT INTO triage_calibration_inputs VALUES (?,?)',(id,t))
                c.execute('INSERT INTO triage_calibration_inputs VALUES (?,?)',(id,t))
            c.execute('INSERT INTO owner_alerts VALUES (?,?)',('gmail:one','high'))
            c.commit();c.close()
            before=db.read_bytes()
            output=io.StringIO()
            exporter.export(db,wiki,30,output,until_ms=30*86400000+2)
            self.assertEqual(db.read_bytes(),before)
            records=[json.loads(s) for s in output.getvalue().splitlines()]
            self.assertEqual(len(records),3)
            self.assertEqual(sum(r['before_high'] for r in records[1:]),1)
            self.assertNotIn('PRIVATE SUBJECT',output.getvalue())
            self.assertEqual(records[0]['priorities'],[['person:matt','high']])

if __name__ == '__main__':
    unittest.main()
