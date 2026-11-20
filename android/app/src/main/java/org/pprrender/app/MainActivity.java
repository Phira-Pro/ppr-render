package org.pprrender.app;

import android.Manifest;
import android.app.Activity;
import android.app.AlertDialog;
import android.content.ContentValues;
import android.content.pm.PackageManager;
import android.database.Cursor;
import android.content.res.Configuration;
import android.graphics.Color;
import android.media.MediaScannerConnection;
import android.net.Uri;
import android.os.Build;
import android.os.Bundle;
import android.os.Environment;
import android.provider.MediaStore;
import android.text.InputType;
import android.util.Log;
import android.view.View;
import android.view.Window;
import android.view.WindowManager;
import android.view.inputmethod.InputMethodManager;
import android.widget.EditText;

import java.io.File;
import java.io.FileInputStream;
import java.io.InputStream;
import java.io.OutputStream;

import quad_native.QuadNative;

/** 只负责 Android 生命周期与文件发布，渲染和交互都在 libppr_render.so。 */
public class MainActivity extends Activity {
    private static final String TAG = "PPRRender";
    private static final int REQUEST_STORAGE = 1001;

    private QuadSurface view;

    @Override
    protected void onCreate(Bundle savedInstanceState) {
        super.onCreate(savedInstanceState);
        requestWindowFeature(Window.FEATURE_NO_TITLE);
        view = new QuadSurface(this);
        setContentView(view);
        applyFullscreen();
        requestStoragePermission();

        // ndk-context 必须先初始化，Rust 的 android::init 才能取到应用目录。
        QuadNative.initializeContext(this);
        QuadNative.activityOnCreate(this);
    }

    @Override
    protected void onResume() {
        super.onResume();
        applyFullscreen();
        QuadNative.activityOnResume();
    }

    @Override
    protected void onPause() {
        super.onPause();
        QuadNative.activityOnPause();
    }

    @Override
    protected void onDestroy() {
        super.onDestroy();
        QuadNative.activityOnDestroy();
    }

    private void applyFullscreen() {
        getWindow().setFlags(
                WindowManager.LayoutParams.FLAG_LAYOUT_NO_LIMITS,
                WindowManager.LayoutParams.FLAG_LAYOUT_NO_LIMITS);
        View decor = getWindow().getDecorView();
        decor.setSystemUiVisibility(
                View.SYSTEM_UI_FLAG_LAYOUT_STABLE
                        | View.SYSTEM_UI_FLAG_LAYOUT_HIDE_NAVIGATION
                        | View.SYSTEM_UI_FLAG_LAYOUT_FULLSCREEN
                        | View.SYSTEM_UI_FLAG_HIDE_NAVIGATION
                        | View.SYSTEM_UI_FLAG_FULLSCREEN
                        | View.SYSTEM_UI_FLAG_IMMERSIVE_STICKY);
    }

    private void requestStoragePermission() {
        java.util.ArrayList<String> permissions = new java.util.ArrayList<>();
        if (checkSelfPermission(Manifest.permission.READ_EXTERNAL_STORAGE)
                != PackageManager.PERMISSION_GRANTED) {
            permissions.add(Manifest.permission.READ_EXTERNAL_STORAGE);
        }
        if (Build.VERSION.SDK_INT <= Build.VERSION_CODES.P
                && checkSelfPermission(Manifest.permission.WRITE_EXTERNAL_STORAGE)
                        != PackageManager.PERMISSION_GRANTED) {
            permissions.add(Manifest.permission.WRITE_EXTERNAL_STORAGE);
        }
        if (!permissions.isEmpty()) {
            requestPermissions(permissions.toArray(new String[0]), REQUEST_STORAGE);
        }
    }

    /** 应用私有文件目录。 */
    public File pprFilesDir() {
        return getFilesDir();
    }

    /** 可写的缓存目录，Rust 的临时文件与 FFmpeg 副本放在这里。 */
    public File pprCacheDir() {
        return getCacheDir();
    }

    /** APK 解压出的原生库目录，里面有 libffmpeg.so。 */
    public File pprNativeLibraryDir() {
        return new File(getApplicationInfo().nativeLibraryDir);
    }

    /** 外部文件目录，用户可通过文件管理器放入回放和谱面。 */
    public File pprExternalFilesDir() {
        File dir = getExternalFilesDir(null);
        return dir != null ? dir : getFilesDir();
    }

    /** 由 miniquad 调用，在渲染线程与 UI 线程之间切换输入法。 */
    public void showKeyboard(final boolean show) {
        runOnUiThread(() -> {
            InputMethodManager imm =
                    (InputMethodManager) getSystemService(INPUT_METHOD_SERVICE);
            if (imm == null) {
                return;
            }
            if (show) {
                view.requestFocus();
                imm.showSoftInput(view, 0);
            } else {
                imm.hideSoftInputFromWindow(view.getWindowToken(), 0);
            }
        });
    }

    /** 由 miniquad 调用。 */
    public void setFullScreen(final boolean fullscreen) {
        runOnUiThread(() -> {
            if (fullscreen) {
                applyFullscreen();
            } else {
                getWindow().clearFlags(WindowManager.LayoutParams.FLAG_LAYOUT_NO_LIMITS);
            }
        });
    }

    /** 由 Rust 在主界面点「退出」时调用；结束 Activity 必须回到 UI 线程。 */
    public void pprQuit() {
        runOnUiThread(this::finish);
    }

    /**
     * 弹出系统输入框，用于 Phira 账号等文本输入。
     *
     * <p>SurfaceView 没有 InputConnection，软键盘的候选词无法直接进入 Rust
     * 的字符队列，所以这里用一个原生对话框收集文本，再通过 JNI 交回。
     */
    public void editText(final String initial, final boolean masked) {
        runOnUiThread(() -> {
            EditText input = new EditText(this);
            input.setText(initial);
            input.setSelection(input.getText().length());
            input.setTextColor(Color.WHITE);
            input.setHintTextColor(Color.GRAY);
            input.setInputType(
                    InputType.TYPE_CLASS_TEXT
                            | (masked
                                    ? InputType.TYPE_TEXT_VARIATION_PASSWORD
                                    : InputType.TYPE_TEXT_VARIATION_NORMAL));
            int pad = (int) (20 * getResources().getDisplayMetrics().density);
            input.setPadding(pad, pad / 2, pad, pad / 2);

            AlertDialog dialog = new AlertDialog.Builder(this)
                    .setTitle(masked ? "输入密码" : "输入内容")
                    .setView(input)
                    .setPositiveButton("确定", (d, which) ->
                            QuadNative.submitInput(input.getText().toString()))
                    .setNegativeButton("取消", null)
                    .create();
            dialog.getWindow().setSoftInputMode(
                    WindowManager.LayoutParams.SOFT_INPUT_STATE_ALWAYS_VISIBLE);
            dialog.show();
        });
    }

    /**
     * 把导出的视频发布到系统的「下载」目录。
     *
     * <p>Android 10 起公共目录只能通过 MediaStore 写入；更老的系统直接
     * 复制到公共目录并通知媒体库扫描。
     *
     * @return 给用户看的保存位置，失败返回 null。
     */
    public String publishVideo(String sourcePath, String displayName) {
        File source = new File(sourcePath);
        if (!source.isFile()) {
            Log.w(TAG, "publishVideo: 源文件不存在 " + sourcePath);
            return null;
        }
        String published = Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q
                ? publishToMediaStore(source, displayName)
                : publishToPublicDownloads(source, displayName);
        if (published != null) {
            return published;
        }
        // 公共下载目录不可写（例如旧系统上用户拒绝了存储权限）时，退回
        // 应用外部目录；文件仍能被文件管理器或 adb 取出，不会只留在缓存里。
        return publishToAppStorage(source, displayName);
    }

    /** Android 10+：通过 MediaStore 写入「下载/ppr-Render」。 */
    private String publishToMediaStore(File source, String displayName) {
        Uri uri = null;
        try {
            ContentValues values = new ContentValues();
            values.put(MediaStore.MediaColumns.DISPLAY_NAME, displayName);
            values.put(MediaStore.MediaColumns.MIME_TYPE, "video/mp4");
            values.put(
                    MediaStore.MediaColumns.RELATIVE_PATH,
                    Environment.DIRECTORY_DOWNLOADS + "/ppr-Render");
            uri = getContentResolver()
                    .insert(MediaStore.Downloads.EXTERNAL_CONTENT_URI, values);
            if (uri == null) {
                Log.w(TAG, "publishVideo: MediaStore 插入失败");
                return null;
            }
            try (OutputStream out = getContentResolver().openOutputStream(uri);
                    InputStream in = new FileInputStream(source)) {
                if (out == null) {
                    throw new IllegalStateException("MediaStore 无法打开输出流");
                }
                copy(in, out);
            }
            String savedName = displayName;
            try (Cursor cursor = getContentResolver().query(
                    uri,
                    new String[] { MediaStore.MediaColumns.DISPLAY_NAME },
                    null,
                    null,
                    null)) {
                if (cursor != null && cursor.moveToFirst()) {
                    savedName = cursor.getString(0);
                }
            }
            return "下载/ppr-Render/" + savedName;
        } catch (Exception e) {
            Log.w(TAG, "publishVideo: MediaStore 写入失败", e);
            // 删除可能已经创建的空条目，避免下载目录里留下 0 字节视频。
            if (uri != null) {
                try {
                    getContentResolver().delete(uri, null, null);
                } catch (Exception ignored) {
                }
            }
            return null;
        }
    }

    /** Android 9 及更早：直接写入公共下载目录并通知媒体库。 */
    @SuppressWarnings("deprecation")
    private String publishToPublicDownloads(File source, String displayName) {
        try {
            File dir = new File(
                    Environment.getExternalStoragePublicDirectory(
                            Environment.DIRECTORY_DOWNLOADS),
                    "ppr-Render");
            if (!dir.isDirectory() && !dir.mkdirs()) {
                Log.w(TAG, "publishVideo: 无法创建 " + dir);
                return null;
            }
            File target = uniqueFile(dir, displayName);
            try (OutputStream out = new java.io.FileOutputStream(target);
                    InputStream in = new FileInputStream(source)) {
                copy(in, out);
            }
            MediaScannerConnection.scanFile(
                    this, new String[] { target.getAbsolutePath() }, null, null);
            return target.getAbsolutePath();
        } catch (Exception e) {
            Log.w(TAG, "publishVideo: 公共下载目录写入失败", e);
            return null;
        }
    }

    /** 最后兜底：应用外部目录，不需要任何存储权限。 */
    private String publishToAppStorage(File source, String displayName) {
        try {
            File root = getExternalFilesDir(Environment.DIRECTORY_MOVIES);
            if (root == null) {
                root = getFilesDir();
            }
            File dir = new File(root, "ppr-Render");
            if (!dir.isDirectory() && !dir.mkdirs()) {
                return null;
            }
            File target = uniqueFile(dir, displayName);
            try (OutputStream out = new java.io.FileOutputStream(target);
                    InputStream in = new FileInputStream(source)) {
                copy(in, out);
            }
            return target.getAbsolutePath();
        } catch (Exception e) {
            Log.w(TAG, "publishVideo: 应用目录写入失败", e);
            return null;
        }
    }

    private static File uniqueFile(File dir, String displayName) {
        File target = new File(dir, displayName);
        if (!target.exists()) {
            return target;
        }
        String stem = displayName;
        String suffix = "";
        int dot = displayName.lastIndexOf('.');
        if (dot > 0) {
            stem = displayName.substring(0, dot);
            suffix = displayName.substring(dot);
        }
        for (int i = 1; ; i++) {
            target = new File(dir, stem + "-" + i + suffix);
            if (!target.exists()) {
                return target;
            }
        }
    }

    private static void copy(InputStream in, OutputStream out) throws Exception {
        byte[] buffer = new byte[1 << 16];
        int read;
        while ((read = in.read(buffer)) > 0) {
            out.write(buffer, 0, read);
        }
    }

    @Override
    public void onConfigurationChanged(Configuration newConfig) {
        super.onConfigurationChanged(newConfig);
        applyFullscreen();
    }
}
