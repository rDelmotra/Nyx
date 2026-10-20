package android.content;

import android.database.Cursor;
import android.net.Uri;
import android.os.ParcelFileDescriptor;
import java.io.InputStream;
import java.io.OutputStream;

/**
 * Test-only shadow, like android.net.Uri next to it. The mockable android.jar
 * makes query/openFileDescriptor final and stubs them to return null, so no test
 * can hand TransferManager a source with a size. Here they are overridable and
 * default to null, i.e. identical to the stub behaviour every other test sees.
 */
public class ContentResolver {
    public ContentResolver(Context context) {}

    public Cursor query(Uri uri, String[] projection, String selection, String[] selectionArgs, String sortOrder) {
        return null;
    }

    public ParcelFileDescriptor openFileDescriptor(Uri uri, String mode) throws java.io.FileNotFoundException {
        return null;
    }

    public InputStream openInputStream(Uri uri) throws java.io.FileNotFoundException {
        return null;
    }

    public OutputStream openOutputStream(Uri uri) throws java.io.FileNotFoundException {
        return null;
    }
}
