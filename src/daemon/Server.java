// A compile server for wsb: loads the Kotlin compiler once and keeps it, so
// each compile skips starting a JVM and warming up the compiler.
//
// Started by wsb as `java -cp <kotlin home>/lib/kotlin-compiler.jar Server.java
// <socket> <idle seconds>`. It listens on a Unix socket in a folder only its
// user can open, and exits after that many seconds with nothing to do.
//
// A request is its length in bytes, as decimal digits and a newline, then
// the words of a command, each ended by a zero byte. (Not the end of the
// client's half of the connection: that can be lost on a connection the
// server has not accepted yet.) The server answers and closes:
//
//   compile <kotlinc arguments...>   answers "OK" or the exit code, a newline,
//                                    then what the compiler printed
//   version                          answers the compiler's version
//   stop                             answers "OK" and exits

import java.io.ByteArrayOutputStream;
import java.io.IOException;
import java.io.InputStream;
import java.io.OutputStream;
import java.io.PrintStream;
import java.lang.reflect.Method;
import java.net.StandardProtocolFamily;
import java.net.UnixDomainSocketAddress;
import java.nio.channels.Channels;
import java.nio.channels.ServerSocketChannel;
import java.nio.channels.SocketChannel;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.nio.file.attribute.BasicFileAttributes;
import java.util.ArrayList;
import java.util.List;
import java.util.concurrent.ExecutorService;
import java.util.concurrent.Executors;
import java.util.concurrent.atomic.AtomicInteger;
import java.util.concurrent.atomic.AtomicLong;

public class Server {
    static Class<?> compilerClass;
    static Method exec;
    static String version;
    static final AtomicLong lastActive = new AtomicLong(System.nanoTime());
    static final AtomicInteger running = new AtomicInteger();

    public static void main(String[] args) throws Exception {
        Path socket = Path.of(args[0]);
        long idleNanos = Long.parseLong(args[1]) * 1_000_000_000L;

        compilerClass = Class.forName("org.jetbrains.kotlin.cli.jvm.K2JVMCompiler");
        exec = compilerClass.getMethod("exec", PrintStream.class, String[].class);
        version = (String) Class.forName("org.jetbrains.kotlin.config.KotlinCompilerVersion")
                .getField("VERSION").get(null);

        ServerSocketChannel server = bind(socket);
        if (server == null) {
            return; // another server is already answering on this socket
        }
        Object ours = key(socket);
        Thread watcher = new Thread(() -> {
            while (true) {
                try {
                    Thread.sleep(10_000);
                } catch (InterruptedException e) {
                    return;
                }
                if (running.get() == 0 && System.nanoTime() - lastActive.get() > idleNanos) {
                    stop(socket, ours);
                }
            }
        });
        watcher.setDaemon(true);
        watcher.start();

        // Compiling recurses deeply, so each request gets a large stack.
        ExecutorService pool = Executors.newCachedThreadPool(r -> {
            Thread t = new Thread(null, r, "wsb-compile", 64L << 20);
            t.setDaemon(true);
            return t;
        });
        while (true) {
            SocketChannel client = server.accept();
            pool.execute(() -> answer(client, socket, ours));
        }
    }

    /// Binds the socket, replacing a file left by a server that is gone.
    /// Returns null if a live server already has it.
    static ServerSocketChannel bind(Path socket) throws IOException {
        for (int attempt = 0; attempt < 2; attempt++) {
            ServerSocketChannel server = ServerSocketChannel.open(StandardProtocolFamily.UNIX);
            try {
                server.bind(UnixDomainSocketAddress.of(socket));
                return server;
            } catch (IOException taken) {
                server.close();
                try (SocketChannel probe = SocketChannel.open(UnixDomainSocketAddress.of(socket))) {
                    return null;
                } catch (IOException dead) {
                    Files.deleteIfExists(socket);
                }
            }
        }
        return null;
    }

    static Object key(Path socket) {
        try {
            return Files.readAttributes(socket, BasicFileAttributes.class).fileKey();
        } catch (IOException e) {
            return null;
        }
    }

    /// Exits, removing the socket only if it is still the one this server made.
    static void stop(Path socket, Object ours) {
        try {
            if (ours != null && ours.equals(key(socket))) {
                Files.deleteIfExists(socket);
            }
        } catch (IOException ignored) {
        }
        System.exit(0);
    }

    static void answer(SocketChannel client, Path socket, Object ours) {
        running.incrementAndGet();
        try (client) {
            InputStream in = Channels.newInputStream(client);
            OutputStream out = Channels.newOutputStream(client);
            List<String> words = words(request(in));
            String command = words.isEmpty() ? "" : words.get(0);
            switch (command) {
                case "compile" -> {
                    ByteArrayOutputStream printed = new ByteArrayOutputStream();
                    String result;
                    try (PrintStream stream = new PrintStream(printed, true, StandardCharsets.UTF_8)) {
                        String[] arguments = words.subList(1, words.size()).toArray(new String[0]);
                        Object code = exec.invoke(compiler(), stream, arguments);
                        result = code.toString();
                    } catch (Throwable t) {
                        result = "CRASHED";
                        printed.writeBytes(String.valueOf(t.getCause() != null ? t.getCause() : t)
                                .getBytes(StandardCharsets.UTF_8));
                    }
                    out.write((result + "\n").getBytes(StandardCharsets.UTF_8));
                    out.write(printed.toByteArray());
                }
                case "version" -> out.write((version + "\n").getBytes(StandardCharsets.UTF_8));
                case "stop" -> {
                    out.write("OK\n".getBytes(StandardCharsets.UTF_8));
                    out.flush();
                    client.close();
                    stop(socket, ours);
                }
                default -> out.write(("unknown command " + command + "\n").getBytes(StandardCharsets.UTF_8));
            }
            out.flush();
        } catch (IOException ignored) {
            // The client went away; nothing to tell it.
        } finally {
            lastActive.set(System.nanoTime());
            running.decrementAndGet();
        }
    }

    /// Reads one request: its length, then that many bytes.
    static byte[] request(InputStream in) throws IOException {
        int length = 0;
        while (true) {
            int b = in.read();
            if (b == '\n') {
                break;
            }
            if (b < '0' || b > '9' || length > 100_000_000) {
                throw new IOException("not a request");
            }
            length = length * 10 + (b - '0');
        }
        byte[] bytes = in.readNBytes(length);
        if (bytes.length != length) {
            throw new IOException("the request ended early");
        }
        return bytes;
    }

    static Object compiler() throws ReflectiveOperationException {
        return compilerClass.getConstructor().newInstance();
    }

    static List<String> words(byte[] bytes) {
        List<String> words = new ArrayList<>();
        int start = 0;
        for (int i = 0; i < bytes.length; i++) {
            if (bytes[i] == 0) {
                words.add(new String(bytes, start, i - start, StandardCharsets.UTF_8));
                start = i + 1;
            }
        }
        return words;
    }
}
