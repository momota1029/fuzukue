package dev.labgui.remote;

import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;
import org.junit.Test;

public class AndroidNoticesTest {
    @Test public void releaseDependenciesHaveBundledNotices() throws IOException {
        for (Path dir = Path.of("").toAbsolutePath(); dir != null; dir = dir.getParent()) {
            for (Path android : new Path[]{dir, dir.resolve("android")}) {
                if (Files.isRegularFile(android.resolve("gradle/libs.versions.toml"))) {
                    AndroidNoticeCheck.verify(android);
                    return;
                }
            }
        }
        throw new AssertionError("Could not find the Android source directory.");
    }
}
