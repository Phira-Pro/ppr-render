package quad_native;

import android.content.Context;
import android.view.Surface;

/**
 * libppr_render.so 的 JNI 入口。
 *
 * <p>类名和包名由 miniquad 的 JNI 命名约定固定（Java_quad_1native_QuadNative_*），
 * 不能更改。
 */
public abstract class QuadNative {
    static {
        System.loadLibrary("ppr_render");
    }

    private QuadNative() {}

    public static native void initializeContext(Context context);

    public static native void activityOnCreate(Object activity);

    public static native void activityOnResume();

    public static native void activityOnPause();

    public static native void activityOnDestroy();

    public static native void surfaceOnSurfaceCreated(Surface surface);

    public static native void surfaceOnSurfaceDestroyed(Surface surface);

    public static native void surfaceOnSurfaceChanged(Surface surface, int width, int height);

    /** @param phase 0=移动 1=抬起 2=按下 3=取消 */
    public static native void surfaceOnTouch(int id, int phase, float x, float y, long time);

    public static native void surfaceOnKeyDown(int keycode);

    public static native void surfaceOnKeyUp(int keycode);

    public static native void surfaceOnCharacter(int character);

    /** 系统输入框确认后，把文本交回 Rust。 */
    public static native void submitInput(String text);
}
