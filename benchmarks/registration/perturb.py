# perturb.py SRC DST [--rot rx,ry,rz] [--t tx,ty,tz] [--drop-top MM] [--drop-bottom MM] [--fov DIAM]
#                    [--truth-in REF.json] [--truth-out TRUTH.json]
# Writes a copy of the series SRC to DST with p' = R(p - c) + c + t applied to ImagePositionPatient and
# ImageOrientationPatient (c = the volume centre, R = Rz Ry Rx in degrees; the pixels are untouched), slices dropped by
# their ORIGINAL z, pixels outside a circular reconstruction field of view of DIAM mm set to -1024 HU, and new
# Frame of Reference, Series and SOP Instance UIDs. --truth-out gets the true moving -> fixed matrix
# (REF * T^-1, REF = --truth-in's matrix or identity).
import sys, os, glob, json, argparse, numpy as np, pydicom
from pydicom.uid import generate_uid
ap = argparse.ArgumentParser(); ap.add_argument('src'); ap.add_argument('dst')
ap.add_argument('--rot', default='0,0,0'); ap.add_argument('--t', default='0,0,0')
ap.add_argument('--drop-top', type=float, default=0); ap.add_argument('--drop-bottom', type=float, default=0)
ap.add_argument('--fov', type=float, default=0); ap.add_argument('--truth-in', default=None); ap.add_argument('--truth-out', default=None)
a = ap.parse_args()
fs = [pydicom.dcmread(f) for f in sorted(glob.glob(a.src + '/*.dcm'))]
fs = [f for f in fs if 'ImagePositionPatient' in f]
zs = np.array([float(f.ImagePositionPatient[2]) for f in fs])
zmax, zmin = zs.max(), zs.min()
keep = [f for f in fs if float(f.ImagePositionPatient[2]) <= zmax - a.drop_top and float(f.ImagePositionPatient[2]) >= zmin + a.drop_bottom]
rx, ry, rz = [np.radians(float(v)) for v in a.rot.split(',')]
Rx = np.array([[1,0,0],[0,np.cos(rx),-np.sin(rx)],[0,np.sin(rx),np.cos(rx)]])
Ry = np.array([[np.cos(ry),0,np.sin(ry)],[0,1,0],[-np.sin(ry),0,np.cos(ry)]])
Rz = np.array([[np.cos(rz),-np.sin(rz),0],[np.sin(rz),np.cos(rz),0],[0,0,1]])
R = Rz @ Ry @ Rx; t = np.array([float(v) for v in a.t.split(',')])
f0 = fs[0]; ps = [float(v) for v in f0.PixelSpacing]
c = np.array([float(f0.ImagePositionPatient[0]) + ps[1]*f0.Columns/2, float(f0.ImagePositionPatient[1]) + ps[0]*f0.Rows/2, (zmin+zmax)/2])
T = np.eye(4); T[:3,:3] = R; T[:3,3] = c + t - R @ c
for_uid = generate_uid(); ser = generate_uid()
os.makedirs(a.dst, exist_ok=True)
for i, f in enumerate(keep):
    ipp = np.array([float(v) for v in f.ImagePositionPatient]); iop = np.array([float(v) for v in f.ImageOrientationPatient])
    if a.fov:
        px = f.pixel_array.astype(np.int32)
        rows, cols = px.shape
        yy, xx = np.mgrid[0:rows, 0:cols]
        X = ipp[0] + xx*ps[1]*iop[0] + yy*ps[0]*iop[3]; Y = ipp[1] + xx*ps[1]*iop[1] + yy*ps[0]*iop[4]
        cx, cy = c[0], c[1]
        out = (X-cx)**2 + (Y-cy)**2 > (a.fov/2)**2
        slope = float(f.get('RescaleSlope', 1)); icpt = float(f.get('RescaleIntercept', 0))
        pad = int(round((-1024 - icpt)/slope))
        lo = -32768 if f.PixelRepresentation == 1 else 0
        px[out] = max(pad, lo)
        f.PixelData = px.astype(f.pixel_array.dtype).tobytes()
    nipp = R @ (ipp - c) + c + t; nrow = R @ iop[:3]; ncol = R @ iop[3:]
    f.ImagePositionPatient = [f'{v:.4f}' for v in nipp]; f.ImageOrientationPatient = [f'{v:.6f}' for v in list(nrow)+list(ncol)]
    f.FrameOfReferenceUID = for_uid; f.SeriesInstanceUID = ser; f.SOPInstanceUID = generate_uid(); f.file_meta.MediaStorageSOPInstanceUID = f.SOPInstanceUID
    f.save_as(os.path.join(a.dst, f'{i:05d}.dcm'))
# truth: M_new = M_ref * T^-1 (M_ref: original moving -> fixed; identity if none)
Mref = np.array(json.load(open(a.truth_in))['matrix']).reshape(4,4) if a.truth_in else np.eye(4)
M = Mref @ np.linalg.inv(T)
if a.truth_out: json.dump({'matrix': [float(v) for v in M.flatten()], 'kept': len(keep), 'of': len(fs)}, open(a.truth_out, 'w'))
print(json.dumps({'kept': len(keep), 'of': len(fs), 'zrange_orig': [zmin, zmax]}))
