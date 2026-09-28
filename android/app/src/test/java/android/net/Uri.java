package android.net;

import java.io.Serializable;
import java.net.URLDecoder;
import java.nio.charset.StandardCharsets;

public abstract class Uri implements Comparable<Uri>, Serializable {

    public abstract String getScheme();
    public abstract String getAuthority();
    public abstract String getPath();
    public abstract String getEncodedPath();
    public abstract Builder buildUpon();

    public static Uri parse(String uriString) {
        return new StringUri(uriString);
    }

    public static String encode(String s) {
        return encode(s, null);
    }

    public static String encode(String s, String allow) {
        if (s == null) return null;
        StringBuilder sb = new StringBuilder();
        for (char c : s.toCharArray()) {
            if ((c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') || (c >= '0' && c <= '9')
                    || "_-!.~'()*".indexOf(c) != -1 || (allow != null && allow.indexOf(c) != -1)) {
                sb.append(c);
            } else {
                sb.append(String.format("%%%02X", (int) c));
            }
        }
        return sb.toString();
    }

    public static String decode(String s) {
        if (s == null) return null;
        try {
            return URLDecoder.decode(s, StandardCharsets.UTF_8.name());
        } catch (Exception e) {
            return s;
        }
    }

    public static final class Builder {
        private String scheme;
        private String authority;
        private String path;

        public Builder scheme(String scheme) {
            this.scheme = scheme;
            return this;
        }

        public Builder authority(String authority) {
            this.authority = authority;
            return this;
        }

        public Builder path(String path) {
            this.path = path;
            return this;
        }

        public Builder encodedPath(String path) {
            this.path = path;
            return this;
        }

        public Uri build() {
            StringBuilder sb = new StringBuilder();
            if (scheme != null) {
                sb.append(scheme).append("://");
            }
            if (authority != null) {
                sb.append(authority);
            }
            if (path != null) {
                if (!path.startsWith("/") && sb.length() > 0) {
                    sb.append('/');
                }
                sb.append(path);
            }
            return new StringUri(sb.toString(), scheme, authority, path);
        }
    }

    private static class StringUri extends Uri {
        private final String uriString;
        private final String scheme;
        private final String authority;
        private final String path;

        StringUri(String uriString) {
            this.uriString = uriString;
            String s = null;
            String a = null;
            String p = null;
            try {
                java.net.URI javaUri = java.net.URI.create(uriString);
                s = javaUri.getScheme();
                a = javaUri.getAuthority();
                p = javaUri.getPath();
            } catch (Exception e) {
                if (uriString != null && uriString.contains("://")) {
                    s = uriString.substring(0, uriString.indexOf("://"));
                    String rest = uriString.substring(uriString.indexOf("://") + 3);
                    int slashIdx = rest.indexOf('/');
                    if (slashIdx != -1) {
                        a = rest.substring(0, slashIdx);
                        p = rest.substring(slashIdx);
                    } else {
                        a = rest;
                        p = "";
                    }
                }
            }
            this.scheme = s;
            this.authority = a;
            this.path = p;
        }

        StringUri(String uriString, String scheme, String authority, String path) {
            this.uriString = uriString;
            this.scheme = scheme;
            this.authority = authority;
            this.path = path;
        }

        @Override public String getScheme() { return scheme; }
        @Override public String getAuthority() { return authority; }
        @Override public String getPath() {
            return path != null ? Uri.decode(path) : null;
        }
        @Override public String getEncodedPath() { return path; }
        @Override public Builder buildUpon() {
            return new Builder().scheme(scheme).authority(authority).encodedPath(path);
        }
        @Override public String toString() { return uriString; }
        @Override public int compareTo(Uri o) { return uriString.compareTo(o.toString()); }
        @Override public boolean equals(Object o) {
            return o instanceof Uri && uriString.equals(o.toString());
        }
        @Override public int hashCode() { return uriString.hashCode(); }
    }
}
