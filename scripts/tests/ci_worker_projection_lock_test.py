"""Actual thread coordination; never a worker freeze or input authority."""
import threading
import time
import unittest
from ci_cargo_parent_signals_test import planner


class WorkerProjectionLockTests(unittest.TestCase):
    def test_pause_waits_until_starting_worker_has_enrolled_and_received_grant(self):
        lock=planner.WorkerProjectionLock();launch=lock.launch(time.monotonic()+2,lambda:None)
        waiting=threading.Event();entered=threading.Event();failures=[]
        def pause():
            try:
                waiting.set()
                with lock.pause(time.monotonic()+2,lambda:None):entered.set()
            except BaseException as error:failures.append(error)
        thread=threading.Thread(target=pause);thread.start()
        self.assertTrue(waiting.wait(1));self.assertFalse(entered.wait(.03))
        launch.close();thread.join(2)
        self.assertFalse(thread.is_alive());self.assertEqual(failures,[]);self.assertTrue(entered.is_set())

    def test_new_launch_waits_until_worker_pause_ends(self):
        lock=planner.WorkerProjectionLock();pause=lock.pause(time.monotonic()+2,lambda:None)
        entered=threading.Event();failures=[]
        def launch():
            try:
                with lock.launch(time.monotonic()+2,lambda:None):entered.set()
            except BaseException as error:failures.append(error)
        thread=threading.Thread(target=launch);thread.start()
        self.assertFalse(entered.wait(.03));pause.close();thread.join(2)
        self.assertFalse(thread.is_alive());self.assertEqual(failures,[]);self.assertTrue(entered.is_set())

    def test_same_thread_cannot_pause_during_an_unfinished_launch(self):
        lock=planner.WorkerProjectionLock()
        with lock.launch(time.monotonic()+1,lambda:None):
            with self.assertRaises(planner.FreshnessHold):lock.pause(time.monotonic()+1,lambda:None)

    def test_cancelled_wait_and_deadline_never_acquire_pause(self):
        lock=planner.WorkerProjectionLock()
        with self.assertRaises(planner.FreshnessHold):lock.pause(time.monotonic()-1,lambda:None)
        def cancelled():raise planner.FreshnessHold('cleanup_uncertain')
        with self.assertRaises(planner.FreshnessHold):lock.launch(time.monotonic()+1,cancelled)

    def test_foreign_thread_cannot_release_original_launch(self):
        lock=planner.WorkerProjectionLock();lease=lock.launch(time.monotonic()+1,lambda:None)
        failures=[]
        def foreign():
            try:lease.close()
            except planner.FreshnessHold:failures.append(True)
        thread=threading.Thread(target=foreign);thread.start();thread.join(1)
        self.assertEqual(failures,[True]);lease.close()
        with lock.pause(time.monotonic()+1,lambda:None):pass


if __name__=='__main__':unittest.main()
