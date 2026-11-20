"""Deterministic unit tests for Milestone 2B: Pure NumPy ByteTrack, TrackPlateVoter, and ParkingSpaceFSM.

Strictly zero-torch runtime with deterministic virtual timestamps (no sleep, millisecond execution).
"""

from __future__ import annotations

import subprocess
import sys
import unittest
from pathlib import Path
from unittest.mock import MagicMock, patch

import numpy as np

VISION_DIR = Path(__file__).resolve().parents[1]
if str(VISION_DIR) not in sys.path:
    sys.path.insert(0, str(VISION_DIR))

from config import VisionConfig
from rtsp_worker import CameraTarget, RtspCameraWorker
from space_tracker import (
    ByteTracker,
    FsmEvent,
    KalmanFilter,
    ParkingSpaceFSM,
    STrack,
    SpaceState,
    TrackPlateVoter,
    box_iou_matrix,
    linear_assignment,
    tlbr_to_xyah,
    xyah_to_tlbr,
)


class TestZeroTorchSpaceTrackerIsolation(unittest.TestCase):
    """Verify that space_tracker operates with zero PyTorch or Ultralytics dependencies."""

    def test_subprocess_space_tracker_imports_no_torch_or_ultralytics(self):
        code = (
            "import sys\n"
            f"sys.path.insert(0, r'{VISION_DIR}')\n"
            "import space_tracker\n"
            "assert 'torch' not in sys.modules, f'torch loaded: {sys.modules.get(\"torch\")}'\n"
            "assert 'ultralytics' not in sys.modules, f'ultralytics loaded: {sys.modules.get(\"ultralytics\")}'\n"
            "print('SPACE_TRACKER_ISOLATION_OK')\n"
        )
        proc = subprocess.run(
            [sys.executable, "-c", code],
            capture_output=True,
            text=True,
            check=False,
        )
        self.assertEqual(
            proc.returncode,
            0,
            f"Process failed with stderr:\n{proc.stderr}\nstdout:\n{proc.stdout}",
        )
        self.assertIn("SPACE_TRACKER_ISOLATION_OK", proc.stdout)


class TestByteTrackerCore(unittest.TestCase):
    """Unit tests for Kalman filter, coordinate conversion, and ByteTrack two-stage association."""

    def setUp(self):
        STrack.reset_counter()

    def test_coordinate_transforms(self):
        tlbr = [100.0, 200.0, 300.0, 400.0]
        xyah = tlbr_to_xyah(tlbr)
        self.assertAlmostEqual(xyah[0], 200.0, places=4)  # cx
        self.assertAlmostEqual(xyah[1], 300.0, places=4)  # cy
        self.assertAlmostEqual(xyah[2], 1.0, places=4)    # a = w/h = 200/200 = 1.0
        self.assertAlmostEqual(xyah[3], 200.0, places=4)  # h

        recon = xyah_to_tlbr(xyah)
        np.testing.assert_allclose(recon, tlbr, atol=1e-4)

    def test_box_iou_matrix(self):
        boxes_a = np.array([[0, 0, 10, 10], [20, 20, 30, 30]], dtype=np.float32)
        boxes_b = np.array([[0, 0, 10, 10], [0, 0, 5, 5]], dtype=np.float32)
        iou = box_iou_matrix(boxes_a, boxes_b)
        self.assertEqual(iou.shape, (2, 2))
        self.assertAlmostEqual(iou[0, 0], 1.0, places=4)
        self.assertAlmostEqual(iou[0, 1], 0.25, places=4)
        self.assertAlmostEqual(iou[1, 0], 0.0, places=4)

    def test_bytetrack_tracking_id_persistence(self):
        tracker = ByteTracker(track_high_thresh=0.45, track_low_thresh=0.20, track_max_lost_sec=3.0)

        # Frame 1: High confidence detection
        det1 = [{"bbox": [100, 100, 300, 300], "confidence": 0.90, "category_type": "CAR"}]
        tracks1 = tracker.update(det1, timestamp=0.0)
        self.assertEqual(len(tracks1), 1)
        initial_id = tracks1[0].track_id
        self.assertGreater(initial_id, 0)

        # Frame 2: Vehicle moved slightly with high confidence -> same track_id
        det2 = [{"bbox": [105, 102, 305, 302], "confidence": 0.88, "category_type": "CAR"}]
        tracks2 = tracker.update(det2, timestamp=0.5)
        self.assertEqual(len(tracks2), 1)
        self.assertEqual(tracks2[0].track_id, initial_id)

        # Frame 3: Temporary low confidence (e.g. shadow/occlusion) -> Second-stage match retains track_id
        det3 = [{"bbox": [110, 105, 310, 305], "confidence": 0.30, "category_type": "CAR"}]
        tracks3 = tracker.update(det3, timestamp=1.0)
        self.assertEqual(len(tracks3), 1)
        self.assertEqual(tracks3[0].track_id, initial_id)

    def test_bytetrack_lost_and_expiration(self):
        tracker = ByteTracker(track_high_thresh=0.45, track_low_thresh=0.20, track_max_lost_sec=3.0)

        det = [{"bbox": [100, 100, 300, 300], "confidence": 0.90, "category_type": "CAR"}]
        tracks = tracker.update(det, timestamp=0.0)
        self.assertEqual(len(tracks), 1)

        # Frame 2: Vehicle disappears for 1.0s -> track becomes lost, not removed yet
        tracks_lost = tracker.update([], timestamp=1.0)
        self.assertEqual(len(tracks_lost), 0)
        self.assertEqual(len(tracker.lost_stracks), 1)

        # Frame 3: Vehicle reappears within 3.0s -> refound with same ID
        tracks_refind = tracker.update(det, timestamp=2.0)
        self.assertEqual(len(tracks_refind), 1)
        self.assertEqual(tracks_refind[0].track_id, tracks[0].track_id)

        # Frame 4: Vehicle disappears for 4.0s (> 3.0s limit) -> track expired & removed
        tracker.update([], timestamp=2.5)
        tracker.update([], timestamp=6.0)
        self.assertEqual(len(tracker.tracked_stracks), 0)
        self.assertEqual(len(tracker.lost_stracks), 0)
        self.assertEqual(len(tracker.removed_stracks), 1)


class TestDeterministicMilestone2BScenarios(unittest.TestCase):
    """5 Deterministic simulation scenarios required by Milestone 2B."""

    def setUp(self):
        STrack.reset_counter()

    def test_scenario_1_drive_through_filtering(self):
        """Scenario 1: 过路车过滤测试.

        A vehicle drives through the space, staying for 2.0s then leaving.
        Verify FSM stays FREE, no VEHICLE_ENTERED events emitted.
        Also test vehicle leaving during ENTERING (< 8s) resets to FREE with 0 events.
        """
        fsm = ParkingSpaceFSM(
            space_id="A-01",
            enter_confirm_sec=3.0,
            occupied_confirm_sec=8.0,
            leave_confirm_sec=5.0,
        )
        track = STrack([100, 100, 300, 300], score=0.9)
        track.track_id = 101

        # Case 1A: Vehicle inside space for 2.0s (< 3.0s enter_confirm_sec)
        ev1, _ = fsm.update(track, timestamp=0.0)
        self.assertEqual(ev1, FsmEvent.NONE)
        self.assertEqual(fsm.state, SpaceState.FREE)

        ev2, _ = fsm.update(track, timestamp=1.0)
        self.assertEqual(ev2, FsmEvent.NONE)
        self.assertEqual(fsm.state, SpaceState.FREE)

        ev3, _ = fsm.update(track, timestamp=2.0)
        self.assertEqual(ev3, FsmEvent.NONE)
        self.assertEqual(fsm.state, SpaceState.FREE)

        # Leaves at t = 2.5s
        ev4, _ = fsm.update(None, timestamp=2.5)
        self.assertEqual(ev4, FsmEvent.NONE)
        self.assertEqual(fsm.state, SpaceState.FREE)
        self.assertIsNone(fsm.occupied_track_id)

        # Case 1B: Vehicle stays for 4.0s (enters ENTERING phase) but leaves at 6.0s (< 8.0s occupied_confirm_sec)
        fsm.reset()
        fsm.update(track, timestamp=0.0)
        fsm.update(track, timestamp=3.1)  # Transitions to ENTERING
        self.assertEqual(fsm.state, SpaceState.ENTERING)

        # Vehicle leaves at t = 6.0s (transient drive-through / U-turn)
        ev_leave, payload = fsm.update(None, timestamp=6.0)
        self.assertEqual(ev_leave, FsmEvent.NONE)
        self.assertIsNone(payload)
        self.assertEqual(fsm.state, SpaceState.FREE)
        self.assertIsNone(fsm.occupied_track_id)

    def test_scenario_2_pre_entry_lock_and_occlusion_inheritance(self):
        """Scenario 2: 进场继承防遮挡测试.

        Vehicle locks plate '京A12345' along trajectory in pre-entry/early frames.
        After parking, the plate is occluded by charging pillar/cables (OCR=None).
        Verify FSM reaches 8.0s, transitions to OCCUPIED, and inherits locked plate & best evidence.
        """
        voter = TrackPlateVoter(min_hits=2, lock_score_thresh=1.50)
        fsm = ParkingSpaceFSM(
            space_id="A-01",
            enter_confirm_sec=3.0,
            occupied_confirm_sec=8.0,
            leave_confirm_sec=5.0,
        )

        dummy_frame_1 = np.ones((100, 100, 3), dtype=np.uint8) * 10
        dummy_frame_best = np.ones((100, 100, 3), dtype=np.uint8) * 200

        track = STrack([100, 100, 300, 300], score=0.92)
        track.track_id = 201

        # Frame 1 in pre-entry area (t = 0.0): good detection
        voter.add_sample(
            201,
            {
                "plate": "京A12345",
                "plate_color": "BLUE",
                "vehicle_type": "FUEL",
                "confidence": 0.90,
                "ocr_confidence": 0.92,
                "laplacian_var": 200.0,
                "plate_area": 4000.0,
            },
            frame=dummy_frame_1,
            timestamp=0.0,
        )
        self.assertFalse(voter.is_locked(201))

        # Frame 2 in pre-entry area (t = 0.5): high-quality snapshot
        locked_p, is_l = voter.add_sample(
            201,
            {
                "plate": "京A12345",
                "plate_color": "BLUE",
                "vehicle_type": "FUEL",
                "confidence": 0.95,
                "ocr_confidence": 0.98,
                "laplacian_var": 290.0,
                "plate_area": 6000.0,
            },
            frame=dummy_frame_best,
            timestamp=0.5,
        )
        self.assertTrue(is_l)
        self.assertEqual(locked_p, "京A12345")
        self.assertTrue(voter.is_locked(201))

        # Vehicle enters space A-01 at t = 1.0, and parks.
        # Plate is now occluded by pillar/cables -> OCR produces nothing for subsequent frames
        fsm.update(track, plate_voter=voter, timestamp=1.0)
        fsm.update(track, plate_voter=voter, timestamp=4.2)  # ENTERING
        self.assertEqual(fsm.state, SpaceState.ENTERING)

        # Reaches 8.0s occupied threshold at t = 9.2s
        ev, payload = fsm.update(track, plate_voter=voter, timestamp=9.2)
        self.assertEqual(ev, FsmEvent.VEHICLE_ENTERED)
        self.assertIsNotNone(payload)
        self.assertEqual(payload["spaceId"], "A-01")
        self.assertTrue(payload["hasVehicle"])
        self.assertEqual(payload["plate"], "京A12345")
        self.assertEqual(payload["plateColor"], "BLUE")
        self.assertEqual(payload["vehicleType"], "FUEL")
        self.assertIs(payload["bestEvidence"], dummy_frame_best)
        self.assertEqual(fsm.state, SpaceState.OCCUPIED)

    def test_scenario_3_unplated_vehicle_and_delayed_plate_resolution(self):
        """Scenario 3: 无牌车/车头朝里测试.

        Vehicle with no plate enters and stays 8s: must transition to OCCUPIED (hasVehicle=True, plate=None).
        Then later, plate is recognized: emits PLATE_UPDATED without losing occupancy.
        """
        voter = TrackPlateVoter(min_hits=2, lock_score_thresh=1.50)
        fsm = ParkingSpaceFSM(
            space_id="A-01",
            enter_confirm_sec=3.0,
            occupied_confirm_sec=8.0,
            leave_confirm_sec=5.0,
        )
        track = STrack([100, 100, 300, 300], score=0.88)
        track.track_id = 301

        # Enters space with no plate detected
        fsm.update(track, plate_voter=voter, timestamp=0.0)
        fsm.update(track, plate_voter=voter, timestamp=3.2)  # ENTERING
        self.assertEqual(fsm.state, SpaceState.ENTERING)

        # Stays for 8.2s -> OCCUPIED confirmed even with NO plate
        ev_enter, payload_enter = fsm.update(track, plate_voter=voter, timestamp=8.2)
        self.assertEqual(ev_enter, FsmEvent.VEHICLE_ENTERED)
        self.assertTrue(payload_enter["hasVehicle"])
        self.assertIsNone(payload_enter["plate"])
        self.assertEqual(fsm.state, SpaceState.OCCUPIED)

        # Later at t = 12.0s, camera captures the plate
        frame_late = np.ones((50, 50, 3), dtype=np.uint8) * 123
        voter.add_sample(
            301,
            {
                "plate": "粤B88888",
                "plate_color": "GREEN",
                "vehicle_type": "NEW_ENERGY",
                "confidence": 0.90,
                "ocr_confidence": 0.95,
                "laplacian_var": 250.0,
                "plate_area": 5000.0,
            },
            frame=frame_late,
            timestamp=12.0,
        )
        voter.add_sample(
            301,
            {
                "plate": "粤B88888",
                "plate_color": "GREEN",
                "vehicle_type": "NEW_ENERGY",
                "confidence": 0.95,
                "ocr_confidence": 0.98,
                "laplacian_var": 280.0,
                "plate_area": 5500.0,
            },
            frame=frame_late,
            timestamp=12.5,
        )
        self.assertTrue(voter.is_locked(301))

        # Next FSM update at t = 13.0s emits PLATE_UPDATED
        ev_up, payload_up = fsm.update(track, plate_voter=voter, timestamp=13.0)
        self.assertEqual(ev_up, FsmEvent.PLATE_UPDATED)
        self.assertEqual(payload_up["plate"], "粤B88888")
        self.assertEqual(payload_up["plateColor"], "GREEN")
        self.assertEqual(payload_up["vehicleType"], "NEW_ENERGY")
        self.assertEqual(fsm.confirmed_plate, "粤B88888")
        self.assertEqual(fsm.state, SpaceState.OCCUPIED)

    def test_scenario_4_single_bad_ocr_noise_rejection(self):
        """Scenario 4: 单帧误识抗噪测试.

        1 bad OCR sample (e.g. 京A12348 with low confidence) + 3 high quality samples (京A12345).
        Verify voter correctly locks 京A12345 and ignores the noise.
        """
        voter = TrackPlateVoter(min_hits=2, lock_score_thresh=1.50)

        # Frame 1: Low quality noise (bad OCR)
        voter.add_sample(
            401,
            {
                "plate": "京A12348",
                "plate_color": "BLUE",
                "confidence": 0.40,
                "ocr_confidence": 0.40,
                "laplacian_var": 20.0,
                "plate_area": 1000.0,
            },
            timestamp=0.0,
        )
        self.assertFalse(voter.is_locked(401))

        # Frame 2: Good sample for real plate 京A12345
        voter.add_sample(
            401,
            {
                "plate": "京A12345",
                "plate_color": "BLUE",
                "confidence": 0.92,
                "ocr_confidence": 0.95,
                "laplacian_var": 250.0,
                "plate_area": 5500.0,
            },
            timestamp=0.5,
        )
        self.assertFalse(voter.is_locked(401))

        # Frame 3: Another good sample for real plate 京A12345 -> Consensus achieved!
        locked_p, is_l = voter.add_sample(
            401,
            {
                "plate": "京A12345",
                "plate_color": "BLUE",
                "confidence": 0.94,
                "ocr_confidence": 0.96,
                "laplacian_var": 270.0,
                "plate_area": 5800.0,
            },
            timestamp=1.0,
        )
        self.assertTrue(is_l)
        self.assertEqual(locked_p, "京A12345")

        l_plate, l_col, _, _, is_locked = voter.get_locked_plate(401)
        self.assertTrue(is_locked)
        self.assertEqual(l_plate, "京A12345")
        self.assertEqual(l_col, "BLUE")

    def test_scenario_5_transient_occlusion_and_leave_debounce(self):
        """Scenario 5: 短暂遮挡防抖测试.

        In OCCUPIED state, vehicle is occluded for 2.0s then reappears -> remains OCCUPIED.
        When vehicle leaves permanently for > 5.0s -> emits VEHICLE_LEFT.
        """
        fsm = ParkingSpaceFSM(
            space_id="A-01",
            enter_confirm_sec=3.0,
            occupied_confirm_sec=8.0,
            leave_confirm_sec=5.0,
        )
        track = STrack([100, 100, 300, 300], score=0.90)
        track.track_id = 501

        # Fast-forward to OCCUPIED state
        fsm.state = SpaceState.OCCUPIED
        fsm.occupied_track_id = 501
        fsm.confirmed_plate = "川A55555"
        fsm.confirmed_color = "BLUE"
        fsm.occupied_since = 100.0

        # Part 5A: Transient occlusion for 2.0s
        # t = 110.0: first missing detection
        ev1, _ = fsm.update(None, timestamp=110.0)
        self.assertEqual(ev1, FsmEvent.NONE)

        # t = 111.2: missing for 1.2s -> transitions to LEAVING debounce
        ev2, _ = fsm.update(None, timestamp=111.2)
        self.assertEqual(ev2, FsmEvent.NONE)
        self.assertEqual(fsm.state, SpaceState.LEAVING)

        # t = 112.0: vehicle reappears (occlusion ended within 2.0s < 5.0s leave_confirm_sec)
        ev3, _ = fsm.update(track, timestamp=112.0)
        self.assertEqual(ev3, FsmEvent.NONE)
        self.assertEqual(fsm.state, SpaceState.OCCUPIED)
        self.assertEqual(fsm.confirmed_plate, "川A55555")

        # Part 5B: Permanent departure
        # t = 120.0: vehicle actually departs
        fsm.update(None, timestamp=120.0)
        fsm.update(None, timestamp=121.2)  # LEAVING
        self.assertEqual(fsm.state, SpaceState.LEAVING)

        # t = 124.0: missing for 4.0s (< 5.0s) -> still LEAVING
        ev4, _ = fsm.update(None, timestamp=124.0)
        self.assertEqual(ev4, FsmEvent.NONE)
        self.assertEqual(fsm.state, SpaceState.LEAVING)

        # t = 125.1: missing for 5.1s (>= 5.0s leave_confirm_sec) -> VACATED!
        ev_left, payload_left = fsm.update(None, timestamp=125.1)
        self.assertEqual(ev_left, FsmEvent.VEHICLE_LEFT)
        self.assertIsNotNone(payload_left)
        self.assertFalse(payload_left["hasVehicle"])
        self.assertEqual(payload_left["plate"], "川A55555")
        self.assertEqual(payload_left["occupiedDurationSeconds"], 25)
        self.assertEqual(fsm.state, SpaceState.FREE)
        self.assertIsNone(fsm.occupied_track_id)
        self.assertIsNone(fsm.confirmed_plate)


class TestRtspWorkerTrackingIntegration(unittest.TestCase):
    """Integration tests for RtspCameraWorker with ByteTracker, TrackPlateVoter, and FSM."""

    def setUp(self):
        STrack.reset_counter()
        self.config = VisionConfig(
            edge_url="http://127.0.0.1:8080",
            debug_draw_boxes=True,
            enter_confirm_sec=3.0,
            occupied_confirm_sec=8.0,
            leave_confirm_sec=5.0,
        )
        self.mock_ai = MagicMock()
        self.target = CameraTarget(
            site_id="site-01",
            space_id="A-01",
            camera_id="CAM-01",
            protocol="RTSP",
            stream_url="rtsp://127.0.0.1:554/main",
            sub_stream_url="rtsp://127.0.0.1:554/sub",
            pre_entry_polygon=[[0.0, 0.0], [1.0, 0.0], [1.0, 0.4], [0.0, 0.4]],
            roi_json='{"spaces": {"A-01": {"polygon": [[0.0, 0.4], [1.0, 0.4], [1.0, 1.0], [0.0, 1.0]]}}}',
        )
        self.worker = RtspCameraWorker(self.target, self.config, self.mock_ai)

    def test_worker_initial_tracking_structures(self):
        self.assertIn("A-01", self.worker._space_fsms)
        self.assertIsInstance(self.worker._tracker, ByteTracker)
        self.assertIsInstance(self.worker._track_voter, TrackPlateVoter)
        status = self.worker.status_dict()
        self.assertEqual(status["spaceId"], "A-01")
        self.assertEqual(status["fsmState"], "FREE")

    def test_worker_dispatch_fsm_event_pushes_detection(self):
        dummy_frame = np.zeros((100, 100, 3), dtype=np.uint8)
        self.worker._session = MagicMock()
        mock_resp = MagicMock()
        mock_resp.status_code = 200
        self.worker._session.post.return_value = mock_resp

        payload = {
            "spaceId": "A-01",
            "hasVehicle": True,
            "plate": "京A12345",
            "plateColor": "BLUE",
            "vehicleType": "FUEL",
            "bestEvidence": dummy_frame,
            "trackId": 1,
        }
        self.worker._dispatch_fsm_event(FsmEvent.VEHICLE_ENTERED, payload, dummy_frame)

        self.assertEqual(self.worker._last_has_vehicle, True)
        self.assertEqual(self.worker._last_plate, "京A12345")
        self.assertEqual(self.worker._last_vehicle_type, "FUEL")
        self.assertIsNotNone(self.worker._latest_plate_jpeg)
        self.assertTrue(self.worker._session.post.called)

    def test_worker_debug_draw_overlay(self):
        dummy_frame = np.zeros((480, 640, 3), dtype=np.uint8)
        spaces_polygons = {
            "A-01": np.array([[100, 100], [500, 100], [500, 400], [100, 400]], dtype=np.int32)
        }
        track = STrack([120, 120, 400, 380], score=0.92)
        track.track_id = 99
        self.worker._pre_entry_poly = np.array([[50, 50], [550, 50], [550, 95], [50, 95]], dtype=np.int32)

        annotated = self.worker._draw_debug_overlay(
            dummy_frame,
            spaces_polygons,
            active_tracks=[track],
            space_to_track={"A-01": track},
        )
        self.assertIsInstance(annotated, np.ndarray)
        self.assertEqual(annotated.shape, dummy_frame.shape)


if __name__ == "__main__":
    unittest.main()
