package dev.photoclean.app;

import android.Manifest;
import android.app.NativeActivity;
import android.app.PendingIntent;
import android.content.ContentUris;
import android.content.Intent;
import android.content.pm.PackageManager;
import android.database.Cursor;
import android.graphics.Bitmap;
import android.net.Uri;
import android.os.Build;
import android.provider.MediaStore;
import android.provider.Settings;
import android.util.Size;

import java.nio.ByteBuffer;
import java.nio.ByteOrder;
import java.util.ArrayList;
import java.util.Arrays;

/**
 * Thin platform shim. All app logic and UI live in Rust (libphotocleaner.so);
 * this class only exposes the Android APIs that need an Activity / Intent
 * result, because NativeActivity has no JNI-free way to receive them.
 */
public class MainActivity extends NativeActivity {
    private static final int REQ_PERMISSION = 1001;
    private static final int REQ_DELETE = 1002;

    private native void nativeOnDeleteResult(boolean ok);
    private native void nativeOnPermissionResult(boolean granted);

    // ---- permissions -------------------------------------------------------
    private boolean granted(String p) {
        return checkSelfPermission(p) == PackageManager.PERMISSION_GRANTED;
    }

    public boolean hasPermission() {
        if (Build.VERSION.SDK_INT >= 33) {
            if (granted(Manifest.permission.READ_MEDIA_IMAGES)) return true;
            // Android 14+: user may grant "selected photos" only.
            return Build.VERSION.SDK_INT >= 34
                    && granted("android.permission.READ_MEDIA_VISUAL_USER_SELECTED");
        }
        return granted(Manifest.permission.READ_EXTERNAL_STORAGE);
    }

    public void requestMediaPermission() {
        runOnUiThread(() -> {
            if (Build.VERSION.SDK_INT >= 34) {
                requestPermissions(new String[]{
                        Manifest.permission.READ_MEDIA_IMAGES,
                        "android.permission.READ_MEDIA_VISUAL_USER_SELECTED"}, REQ_PERMISSION);
            } else if (Build.VERSION.SDK_INT >= 33) {
                requestPermissions(new String[]{Manifest.permission.READ_MEDIA_IMAGES}, REQ_PERMISSION);
            } else {
                requestPermissions(new String[]{Manifest.permission.READ_EXTERNAL_STORAGE}, REQ_PERMISSION);
            }
        });
    }

    public void openAppSettings() {
        runOnUiThread(() -> {
            Intent i = new Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS,
                    Uri.fromParts("package", getPackageName(), null));
            startActivity(i);
        });
    }

    @Override
    public void onRequestPermissionsResult(int code, String[] perms, int[] results) {
        super.onRequestPermissionsResult(code, perms, results);
        if (code == REQ_PERMISSION) nativeOnPermissionResult(hasPermission());
    }

    // ---- MediaStore --------------------------------------------------------
    /** All image ids, newest first. Stable ids, not positions. */
    public long[] queryIds() {
        String[] proj = {MediaStore.Images.Media._ID};
        String sort = MediaStore.Images.Media.DATE_ADDED + " DESC, " + MediaStore.Images.Media._ID + " DESC";
        try (Cursor c = getContentResolver().query(
                MediaStore.Images.Media.EXTERNAL_CONTENT_URI, proj, null, null, sort)) {
            if (c == null) return new long[0];
            long[] out = new long[c.getCount()];
            int col = c.getColumnIndexOrThrow(MediaStore.Images.Media._ID);
            int n = 0;
            while (c.moveToNext() && n < out.length) out[n++] = c.getLong(col);
            return n == out.length ? out : Arrays.copyOf(out, n);
        } catch (Throwable t) {
            return new long[0];
        }
    }

    /** Layout: int32 LE width, int32 LE height, then RGBA8888 (premultiplied) pixels. */
    public byte[] loadThumb(long id, int w, int h) {
        try {
            Uri uri = ContentUris.withAppendedId(MediaStore.Images.Media.EXTERNAL_CONTENT_URI, id);
            Bitmap b = getContentResolver().loadThumbnail(uri, new Size(w, h), null);
            if (b.getConfig() != Bitmap.Config.ARGB_8888) {
                Bitmap c = b.copy(Bitmap.Config.ARGB_8888, false);
                b.recycle();
                b = c;
            }
            int bw = b.getWidth(), bh = b.getHeight();
            ByteBuffer buf = ByteBuffer.allocate(8 + bw * bh * 4).order(ByteOrder.LITTLE_ENDIAN);
            buf.putInt(bw).putInt(bh);
            b.copyPixelsToBuffer(buf);
            b.recycle();
            return buf.array();
        } catch (Throwable t) {
            return null;
        }
    }

    // ---- deletion (system confirmation, API 30+) ---------------------------
    public void requestDelete(long[] ids) {
        runOnUiThread(() -> {
            try {
                ArrayList<Uri> uris = new ArrayList<>(ids.length);
                for (long id : ids) {
                    uris.add(ContentUris.withAppendedId(MediaStore.Images.Media.EXTERNAL_CONTENT_URI, id));
                }
                PendingIntent pi = MediaStore.createDeleteRequest(getContentResolver(), uris);
                startIntentSenderForResult(pi.getIntentSender(), REQ_DELETE, null, 0, 0, 0);
            } catch (Throwable t) {
                nativeOnDeleteResult(false);
            }
        });
    }

    @Override
    protected void onActivityResult(int req, int res, Intent data) {
        super.onActivityResult(req, res, data);
        if (req == REQ_DELETE) nativeOnDeleteResult(res == RESULT_OK);
    }
}
