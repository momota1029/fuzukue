package dev.labgui.remote;

import java.io.IOException;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.HashMap;
import java.util.HashSet;
import java.util.Map;
import java.util.Set;
import java.util.TreeSet;
import java.util.regex.Matcher;
import java.util.regex.Pattern;

// Shared by the JVM test and the release check. Reads the catalog's inline tables
// and the app's release dependency declarations without evaluating Gradle.
public final class AndroidNoticeCheck {
    private static final String MODULE = "[\\w.-]+:[\\w.-]+";
    private static final String VERSION = "[\\w.+-]+";
    private static final Pattern ARTIFACT = Pattern.compile(
        "^Artifact: (" + MODULE + "):(" + VERSION + ")[ \\t]*$", Pattern.MULTILINE);
    private static final Pattern ASSIGNMENT = Pattern.compile("^([\\w-]+)\\s*=\\s*(.+)$");
    private static final Pattern COMMENTS_OR_STRINGS =
        Pattern.compile("/\\*[\\s\\S]*?\\*/|//[^\\r\\n]*|\"(?:\\\\.|[^\"\\\\])*\"");
    private static final String CONFIGURATION = "(?:implementation|api|runtimeOnly|releaseImplementation|releaseApi|releaseRuntimeOnly)";
    private static final Pattern RUNTIME_CALL = Pattern.compile(
        "\\b(?:" + CONFIGURATION + "\\s*\\(|add\\s*\\(\\s*\"" + CONFIGURATION + "\"\\s*,)");
    private static final Pattern ARGUMENT = Pattern.compile(
        "\\s*(?:(?:enforcedPlatform|platform)\\s*\\(\\s*)?"
        + "(?:libs\\.([\\w.]+)|\"(" + MODULE + ")(?::(" + VERSION + "))?\")\\s*\\)");
    private static final Pattern SHORTHAND = Pattern.compile("\"(" + MODULE + "):(" + VERSION + ")\"");

    public static void main(String[] args) throws IOException {
        System.out.println("Android notices cover " + verify(Path.of(args[0])) + " release dependency declarations.");
    }

    static int verify(Path android) throws IOException {
        Path assets = android.resolve("app/src/main/assets/licenses");
        for (String name : new String[]{"THIRD_PARTY_LICENSES.txt", "NOTICE.txt", "KaTeX-OFL-1.1.txt"}) {
            Path file = assets.resolve(name);
            if (!Files.isRegularFile(file) || Files.size(file) == 0) {
                throw new IllegalStateException("Missing or empty APK notice: " + name);
            }
        }

        Set<String> artifacts = new HashSet<>();
        Set<String> versionedArtifacts = new HashSet<>();
        Matcher artifact = ARTIFACT.matcher(Files.readString(assets.resolve("THIRD_PARTY_LICENSES.txt")));
        while (artifact.find()) {
            artifacts.add(artifact.group(1));
            versionedArtifacts.add(artifact.group(1) + ":" + artifact.group(2));
        }

        Map<String, String> versions = new HashMap<>();
        Map<String, String> libraries = new HashMap<>();
        String section = "";
        for (String rawLine : Files.readAllLines(android.resolve("gradle/libs.versions.toml"))) {
            String line = rawLine.split("#", 2)[0].strip();
            if (line.startsWith("[")) {
                section = line;
            }
            Matcher assignment = ASSIGNMENT.matcher(line);
            if (!assignment.matches()) {
                continue;
            }
            if (section.equals("[versions]")) {
                versions.put(assignment.group(1), assignment.group(2).replace("\"", ""));
            } else if (section.equals("[libraries]")) {
                libraries.put(assignment.group(1).replace('-', '.').replace('_', '.'), assignment.group(2));
            }
        }

        String gradle = COMMENTS_OR_STRINGS.matcher(Files.readString(android.resolve("app/build.gradle.kts")))
            .replaceAll(match -> match.group().startsWith("/") ? " " : Matcher.quoteReplacement(match.group()));
        Matcher call = RUNTIME_CALL.matcher(gradle);
        Set<String> dependencies = new TreeSet<>();
        Set<String> missing = new TreeSet<>();
        while (call.find()) {
            Matcher argument = ARGUMENT.matcher(gradle).region(call.end(), gradle.length());
            if (!argument.lookingAt()) {
                throw new IllegalStateException("Unsupported Gradle runtime dependency: "
                    + gradle.substring(call.start()).lines().findFirst().orElse(""));
            }
            String alias = argument.group(1);
            String module = argument.group(2);
            String version = argument.group(3);
            if (alias != null) {
                String entry = libraries.getOrDefault(alias, "");
                Matcher shorthand = SHORTHAND.matcher(entry);
                if (shorthand.matches()) {
                    module = shorthand.group(1);
                    version = shorthand.group(2);
                } else {
                    module = field(entry, "module");
                    String group = field(entry, "group");
                    String name = field(entry, "name");
                    if (module == null && group != null && name != null) {
                        module = group + ":" + name;
                    }
                    version = field(entry, "version");
                    String ref = field(entry, "version.ref");
                    if (ref != null) {
                        version = versions.get(ref);
                        if (version == null) {
                            throw new IllegalStateException("Unknown catalog version reference: " + ref);
                        }
                    }
                }
                if (module == null) {
                    throw new IllegalStateException("Missing or unsupported release library in catalog: libs." + alias);
                }
            }
            String coordinate = version == null ? module : module + ":" + version;
            dependencies.add(coordinate);
            if (!(version == null ? artifacts : versionedArtifacts).contains(coordinate)) {
                missing.add(coordinate);
            }
        }
        if (dependencies.isEmpty() || !missing.isEmpty()) {
            throw new IllegalStateException("Missing Android license entries or runtime declarations: " + missing);
        }
        return dependencies.size();
    }

    private static String field(String entry, String key) {
        Matcher field = Pattern.compile("(?:\\{|,)\\s*" + Pattern.quote(key) + "\\s*=\\s*\"([^\"]+)\"").matcher(entry);
        return field.find() ? field.group(1) : null;
    }
}
