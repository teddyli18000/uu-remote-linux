#define _GNU_SOURCE
#include <dirent.h>
#include <errno.h>
#include <fcntl.h>
#include <limits.h>
#include <signal.h>
#include <stdbool.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/file.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>

#ifndef PATH_MAX
#define PATH_MAX 4096
#endif

#define STATE_NAME "state"
#define LOCK_NAME "lock"
#define TEMP_NAME ".state.tmp"
#define MAX_STATE 256

static char sys_root[PATH_MAX] = "/sys";
static char state_dir[PATH_MAX] = "/run/uuremote-display-helper";

struct owned_state {
    uid_t uid;
    char connector[128];
};

static uid_t trusted_uid(void)
{
#ifdef UU_HELPER_TEST
    return geteuid();
#else
    return 0;
#endif
}

static void timeout_handler(int sig)
{
    static const char message[] = "operation timed out\n";
    (void)sig;
    ssize_t written = write(STDERR_FILENO, message, sizeof(message) - 1);
    (void)written;
    _exit(2);
}

static int fail(void)
{
    static const char message[] = "operation failed\n";
    ssize_t written = write(STDERR_FILENO, message, sizeof(message) - 1);
    (void)written;
    return 1;
}

static bool path_join(char *out, size_t cap, const char *a, const char *b)
{
    int n = snprintf(out, cap, "%s/%s", a, b);
    return n >= 0 && (size_t)n < cap;
}

static bool safe_component(const char *s)
{
    size_t i;
    if (!s || !*s || strlen(s) >= 128)
        return false;
    for (i = 0; s[i]; ++i) {
        unsigned char c = (unsigned char)s[i];
        if (!((c >= 'a' && c <= 'z') || (c >= 'A' && c <= 'Z') ||
              (c >= '0' && c <= '9') || c == '-' || c == '_'))
            return false;
    }
    return true;
}

static bool parse_number(const char *s, unsigned long *value)
{
    unsigned long n = 0;
    const unsigned char *p = (const unsigned char *)s;
    if (!*p)
        return false;
    for (; *p; ++p) {
        if (*p < '0' || *p > '9')
            return false;
        if (n > (ULONG_MAX - (unsigned long)(*p - '0')) / 10)
            return false;
        n = n * 10 + (unsigned long)(*p - '0');
    }
    *value = n;
    return true;
}

static bool parse_card_name(const char *name, unsigned long *card)
{
    if (strncmp(name, "card", 4) != 0)
        return false;
    return parse_number(name + 4, card);
}

static bool parse_connector_name(const char *name, unsigned long *card,
                                 const char **kind)
{
    const char *dash;
    char number[32];
    size_t len;
    if (strncmp(name, "card", 4) != 0)
        return false;
    dash = strchr(name + 4, '-');
    if (!dash)
        return false;
    len = (size_t)(dash - (name + 4));
    if (len == 0 || len >= sizeof(number))
        return false;
    memcpy(number, name + 4, len);
    number[len] = '\0';
    if (!parse_number(number, card) || !safe_component(dash + 1))
        return false;
    *kind = dash + 1;
    return true;
}

static bool parse_hdmi_port(const char *kind, unsigned long *port)
{
    static const char prefix[] = "HDMI-A-";
    if (strncmp(kind, prefix, sizeof(prefix) - 1) != 0)
        return false;
    return parse_number(kind + sizeof(prefix) - 1, port);
}

static bool checked_stat(const char *path, struct stat *st, bool directory,
                         bool private_file)
{
    if (stat(path, st) != 0 || st->st_uid != trusted_uid())
        return false;
    if (directory ? !S_ISDIR(st->st_mode) : !S_ISREG(st->st_mode))
        return false;
    if (private_file ? (st->st_mode & 077) != 0 : (st->st_mode & 0022) != 0)
        return false;
    return true;
}

static bool read_small_file(const char *path, char *buf, size_t cap)
{
    int fd = -1;
    struct stat st;
    ssize_t n;
    bool ok = false;
    if (cap < 2)
        return false;
    fd = open(path, O_RDONLY | O_CLOEXEC | O_NOFOLLOW);
    if (fd < 0 || fstat(fd, &st) != 0 || st.st_uid != trusted_uid() ||
        !S_ISREG(st.st_mode) || (st.st_mode & 0022) != 0)
        goto done;
    n = read(fd, buf, cap - 1);
    if (n < 0 || (size_t)n >= cap - 1)
        goto done;
    buf[n] = '\0';
    while (n > 0 && (buf[n - 1] == '\n' || buf[n - 1] == '\r'))
        buf[--n] = '\0';
    ok = true;
done:
    if (fd >= 0)
        close(fd);
    return ok;
}

static bool canonical_devices(char *out, size_t cap)
{
    char input[PATH_MAX];
    char root[PATH_MAX];
    struct stat st;
    if (!path_join(input, sizeof(input), sys_root, "devices") ||
        !realpath(input, root) || !checked_stat(root, &st, true, false))
        return false;
    if (strlen(root) >= cap)
        return false;
    strcpy(out, root);
    return true;
}

static bool trusted_target_dir(const char *path, char *out, size_t cap)
{
    char devices[PATH_MAX];
    char resolved[PATH_MAX];
    struct stat st;
    size_t n;
    if (!canonical_devices(devices, sizeof(devices)) || !realpath(path, resolved) ||
        !checked_stat(resolved, &st, true, false))
        return false;
    n = strlen(devices);
    if (strncmp(resolved, devices, n) != 0 || resolved[n] != '/')
        return false;
    if (strlen(resolved) >= cap)
        return false;
    strcpy(out, resolved);
    return true;
}

static bool connector_dir(const char *name, char *out, size_t cap,
                          unsigned long *card_out, const char **kind_out)
{
    unsigned long card;
    const char *kind;
    char class_dir[PATH_MAX];
    char class_path[PATH_MAX];
    struct stat lst;
    if (!parse_connector_name(name, &card, &kind) ||
        !path_join(class_dir, sizeof(class_dir), sys_root, "class/drm") ||
        !path_join(class_path, sizeof(class_path), class_dir, name) ||
        lstat(class_path, &lst) != 0 || !S_ISLNK(lst.st_mode) ||
        lst.st_uid != trusted_uid() ||
        !trusted_target_dir(class_path, out, cap))
        return false;
    if (card_out)
        *card_out = card;
    if (kind_out)
        *kind_out = kind;
    return true;
}

static bool connector_status(const char *name, char *status, size_t cap,
                             unsigned long *card_out, const char **kind_out,
                             char *dir_out, size_t dir_cap)
{
    char dir[PATH_MAX];
    char path[PATH_MAX];
    const char *kind;
    unsigned long card;
    if (!connector_dir(name, dir, sizeof(dir), &card, &kind) ||
        !path_join(path, sizeof(path), dir, "status") ||
        !read_small_file(path, status, cap))
        return false;
    if (card_out)
        *card_out = card;
    if (kind_out)
        *kind_out = kind;
    if (dir_out) {
        if (strlen(dir) >= dir_cap)
            return false;
        strcpy(dir_out, dir);
    }
    return true;
}

static bool connector_force_state(const char *name, char *force, size_t cap)
{
    char path[PATH_MAX];
    char card_dir[PATH_MAX];
    char connector_dir_path[PATH_MAX];
    char card_text[32];
    unsigned long card;
    unsigned long port;
    const char *kind;
    int n;
    if (!parse_connector_name(name, &card, &kind) ||
        !parse_hdmi_port(kind, &port) ||
        !path_join(path, sizeof(path), sys_root, "kernel/debug/dri"))
        return false;
    (void)port;
    n = snprintf(card_text, sizeof(card_text), "%lu", card);
    if (n < 0 || (size_t)n >= sizeof(card_text) ||
        !path_join(card_dir, sizeof(card_dir), path, card_text) ||
        !path_join(connector_dir_path, sizeof(connector_dir_path), card_dir, kind) ||
        !path_join(path, sizeof(path), connector_dir_path, "force"))
        return false;
    return read_small_file(path, force, cap);
}

static bool connector_write(const char *name, const char *value);
static bool delete_state(int dirfd);

static bool clear_owned_force(int dirfd, const struct owned_state *state)
{
    char force[64];
    if (!connector_force_state(state->connector, force, sizeof(force)))
        return false;
    if (strcmp(force, "on") == 0) {
        if (!connector_write(state->connector, "detect\n") ||
            !connector_force_state(state->connector, force, sizeof(force)) ||
            strcmp(force, "unspecified") != 0)
            return false;
    } else if (strcmp(force, "unspecified") != 0) {
        return false;
    }
    return delete_state(dirfd);
}

#ifdef UU_HELPER_TEST
static bool update_fixture_force_state(const char *name, const char *value)
{
    char path[PATH_MAX];
    char card_dir[PATH_MAX];
    char connector_dir_path[PATH_MAX];
    char card_text[32];
    const char *fixture_value;
    const char *kind;
    unsigned long card;
    int fd = -1;
    int n;
    bool ok = false;
    if (strcmp(value, "on\n") == 0)
        fixture_value = "on\n";
    else if (strcmp(value, "detect\n") == 0)
        fixture_value = "unspecified\n";
    else
        return false;
    if (!parse_connector_name(name, &card, &kind))
        return false;
    n = snprintf(card_text, sizeof(card_text), "%lu", card);
    if (n < 0 || (size_t)n >= sizeof(card_text) ||
        !path_join(path, sizeof(path), sys_root, "kernel/debug/dri") ||
        !path_join(card_dir, sizeof(card_dir), path, card_text) ||
        !path_join(connector_dir_path, sizeof(connector_dir_path), card_dir, kind) ||
        !path_join(path, sizeof(path), connector_dir_path, "force"))
        return false;
    fd = open(path, O_WRONLY | O_CLOEXEC | O_NOFOLLOW);
    if (fd < 0 || ftruncate(fd, 0) != 0)
        goto done;
    n = (int)strlen(fixture_value);
    ok = write(fd, fixture_value, (size_t)n) == n;
done:
    if (fd >= 0)
        close(fd);
    return ok;
}
#endif

static bool connector_write(const char *name, const char *value)
{
    char dir[PATH_MAX];
    char path[PATH_MAX];
    struct stat st;
    int fd = -1;
    ssize_t n;
    bool ok = false;
#ifdef UU_HELPER_TEST
    const char *requested_value = value;
#endif
    if (!connector_dir(name, dir, sizeof(dir), NULL, NULL) ||
        !path_join(path, sizeof(path), dir, "status"))
        return false;
    fd = open(path, O_WRONLY | O_CLOEXEC | O_NOFOLLOW);
    if (fd < 0 || fstat(fd, &st) != 0 || st.st_uid != trusted_uid() ||
        !S_ISREG(st.st_mode) || (st.st_mode & 0022) != 0)
        goto done;
#ifdef UU_HELPER_TEST
    /* Regular files stand in for sysfs attributes in the isolated fixture. */
    {
        const char *fixture_value = value;
        if (strcmp(value, "on\n") == 0)
            fixture_value = "connected\n";
        else if (strcmp(value, "detect\n") == 0)
            fixture_value = "disconnected\n";
        if (ftruncate(fd, 0) != 0)
            goto done;
        value = fixture_value;
    }
#endif
    n = write(fd, value, strlen(value));
    ok = n == (ssize_t)strlen(value);
#ifdef UU_HELPER_TEST
    if (ok)
        ok = update_fixture_force_state(name, requested_value);
#endif
done:
    if (fd >= 0)
        close(fd);
    return ok;
}

static bool scan_connectors(bool *connected, bool *unknown,
                            unsigned long target_card, bool select_candidate,
                            unsigned long *candidate_port, char *candidate,
                            size_t candidate_cap)
{
    char class_dir[PATH_MAX];
    DIR *dir = NULL;
    struct dirent *entry;
    bool ok = false;
    if (!path_join(class_dir, sizeof(class_dir), sys_root, "class/drm"))
        return false;
    dir = opendir(class_dir);
    if (!dir)
        return false;
    for (;;) {
        unsigned long card, port;
        const char *kind;
        char status[64];
        errno = 0;
        entry = readdir(dir);
        if (!entry) {
            ok = errno == 0;
            break;
        }
        if (strncmp(entry->d_name, "card", 4) != 0)
            continue;
        if (!strchr(entry->d_name + 4, '-'))
            continue;
        if (!parse_connector_name(entry->d_name, &card, &kind)) {
            *unknown = true;
            continue;
        }
        if (!connector_status(entry->d_name, status, sizeof(status), NULL, NULL,
                              NULL, 0)) {
            *unknown = true;
            continue;
        }
        if (strcmp(status, "connected") == 0)
            *connected = true;
        else if (strcmp(status, "disconnected") != 0)
            *unknown = true;
        if (select_candidate && card == target_card &&
            strcmp(status, "disconnected") == 0 && parse_hdmi_port(kind, &port) &&
            (candidate[0] == '\0' || port < *candidate_port)) {
            int n = snprintf(candidate, candidate_cap, "%s", entry->d_name);
            if (n < 0 || (size_t)n >= candidate_cap) {
                *unknown = true;
                continue;
            }
            *candidate_port = port;
        }
    }
    closedir(dir);
    return ok;
}

static bool card_driver(const char *card_name, char *driver, size_t cap)
{
    char class_dir[PATH_MAX];
    char card_path[PATH_MAX];
    char path[PATH_MAX];
    char resolved[PATH_MAX];
    char root[PATH_MAX];
    struct stat lst, st;
    const char *base;
    if (!path_join(class_dir, sizeof(class_dir), sys_root, "class/drm") ||
        !path_join(card_path, sizeof(card_path), class_dir, card_name) ||
        lstat(card_path, &lst) != 0 || !S_ISLNK(lst.st_mode) ||
        lst.st_uid != trusted_uid())
        return false;
    if (!path_join(path, sizeof(path), card_path, "device/driver") ||
        !realpath(path, resolved) || !checked_stat(resolved, &st, true, false))
        return false;
    if (!path_join(root, sizeof(root), sys_root, "bus"))
        return false;
    if (strncmp(resolved, root, strlen(root)) != 0 ||
        resolved[strlen(root)] != '/')
        return false;
    base = strrchr(resolved, '/');
    if (!base || !safe_component(base + 1) || strlen(base + 1) >= cap)
        return false;
    strcpy(driver, base + 1);
    return true;
}

static bool find_i915_card(unsigned long *card_out, unsigned *count_out)
{
    char class_dir[PATH_MAX];
    DIR *dir = NULL;
    struct dirent *entry;
    unsigned count = 0;
    unsigned long selected = 0;
    bool ok = false;
    if (!path_join(class_dir, sizeof(class_dir), sys_root, "class/drm"))
        return false;
    dir = opendir(class_dir);
    if (!dir)
        return false;
    for (;;) {
        unsigned long id;
        char driver[128];
        errno = 0;
        entry = readdir(dir);
        if (!entry) {
            ok = errno == 0;
            break;
        }
        if (!parse_card_name(entry->d_name, &id))
            continue;
        if (!card_driver(entry->d_name, driver, sizeof(driver)))
            goto done;
        if (strcmp(driver, "i915") == 0) {
            ++count;
            selected = id;
        }
    }
    if (ok) {
        *card_out = selected;
        *count_out = count;
    }
done:
    closedir(dir);
    return ok;
}

static bool validate_owned_connector(const char *name, char *status,
                                     size_t status_cap)
{
    unsigned long card;
    const char *kind;
    char driver[128];
    char card_name[64];
    unsigned long unused;
    if (!parse_connector_name(name, &card, &kind) ||
        !parse_hdmi_port(kind, &unused))
        return false;
    if (snprintf(card_name, sizeof(card_name), "card%lu", card) < 0 ||
        !card_driver(card_name, driver, sizeof(driver)) ||
        strcmp(driver, "i915") != 0)
        return false;
    return connector_status(name, status, status_cap, NULL, NULL, NULL, 0);
}

static bool mkdir_state_dir(bool create)
{
    struct stat st;
#ifndef UU_HELPER_TEST
    if (lstat("/run", &st) != 0 || !S_ISDIR(st.st_mode) ||
        st.st_uid != 0 || (st.st_mode & 0022) != 0)
        return false;
#endif
    if (lstat(state_dir, &st) != 0) {
        if (errno != ENOENT || !create || mkdir(state_dir, 0700) != 0)
            return false;
        if (lstat(state_dir, &st) != 0)
            return false;
    }
    return st.st_uid == trusted_uid() && S_ISDIR(st.st_mode) &&
           (st.st_mode & 077) == 0;
}

static bool open_state_dir(bool create, int *dirfd_out)
{
    struct stat before, after;
    int fd;
    if (!mkdir_state_dir(create) || lstat(state_dir, &before) != 0)
        return false;
    fd = open(state_dir, O_RDONLY | O_DIRECTORY | O_CLOEXEC | O_NOFOLLOW);
    if (fd < 0 || fstat(fd, &after) != 0 || after.st_uid != trusted_uid() ||
        !S_ISDIR(after.st_mode) || (after.st_mode & 077) != 0 ||
        before.st_dev != after.st_dev || before.st_ino != after.st_ino) {
        if (fd >= 0)
            close(fd);
        return false;
    }
    *dirfd_out = fd;
    return true;
}

static bool private_file_stat(int dirfd, const char *name, struct stat *st)
{
    if (fstatat(dirfd, name, st, AT_SYMLINK_NOFOLLOW) != 0 ||
        st->st_uid != trusted_uid() || !S_ISREG(st->st_mode) ||
        (st->st_mode & 077) != 0 || st->st_nlink != 1)
        return false;
    return true;
}

static bool read_state(int dirfd, struct owned_state *state, bool *exists)
{
    char buf[MAX_STATE];
    char uid_text[32];
    char connector[128];
    struct stat st;
    int fd = -1;
    ssize_t n;
    unsigned long uid_value;
    *exists = false;
    if (fstatat(dirfd, STATE_NAME, &st, AT_SYMLINK_NOFOLLOW) != 0)
        return errno == ENOENT;
    if (!private_file_stat(dirfd, STATE_NAME, &st))
        return false;
    fd = openat(dirfd, STATE_NAME, O_RDONLY | O_CLOEXEC | O_NOFOLLOW);
    if (fd < 0 || fstat(fd, &st) != 0 || st.st_uid != trusted_uid() ||
        !S_ISREG(st.st_mode) || (st.st_mode & 077) != 0 || st.st_nlink != 1)
        goto bad;
    n = read(fd, buf, sizeof(buf) - 1);
    if (n <= 0 || n >= (ssize_t)sizeof(buf) - 1)
        goto bad;
    buf[n] = '\0';
    if (sscanf(buf, "uid=%31[0-9]\nconnector=%127[A-Za-z0-9_-]\n", uid_text,
               connector) != 2 || !parse_number(uid_text, &uid_value) ||
        uid_value > (unsigned long)(uid_t)-1 || !safe_component(connector))
        goto bad;
    {
        char expected[MAX_STATE];
        int expected_n = snprintf(expected, sizeof(expected), "uid=%s\nconnector=%s\n",
                                  uid_text, connector);
        if (expected_n < 0 || expected_n != n || memcmp(expected, buf, (size_t)n) != 0)
            goto bad;
    }
    state->uid = (uid_t)uid_value;
    strcpy(state->connector, connector);
    *exists = true;
    close(fd);
    return true;
bad:
    if (fd >= 0)
        close(fd);
    return false;
}

static bool remove_stale_temp(int dirfd)
{
    struct stat st;
    if (fstatat(dirfd, TEMP_NAME, &st, AT_SYMLINK_NOFOLLOW) != 0)
        return errno == ENOENT;
    if (!private_file_stat(dirfd, TEMP_NAME, &st))
        return false;
    return unlinkat(dirfd, TEMP_NAME, 0) == 0;
}

static bool write_state(int dirfd, const struct owned_state *state)
{
    char buf[MAX_STATE];
    int fd = -1;
    int n = snprintf(buf, sizeof(buf), "uid=%lu\nconnector=%s\n",
                     (unsigned long)state->uid, state->connector);
    bool ok = false;
    if (n < 0 || (size_t)n >= sizeof(buf) || !remove_stale_temp(dirfd))
        return false;
    fd = openat(dirfd, TEMP_NAME,
                O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC | O_NOFOLLOW, 0600);
    if (fd < 0)
        return false;
    if (fchown(fd, trusted_uid(), (gid_t)-1) != 0 || fchmod(fd, 0600) != 0 ||
        write(fd, buf, (size_t)n) != n || fsync(fd) != 0)
        goto done;
    if (renameat(dirfd, TEMP_NAME, dirfd, STATE_NAME) != 0 || fsync(dirfd) != 0)
        goto done;
    ok = true;
done:
    close(fd);
    if (!ok)
        (void)unlinkat(dirfd, TEMP_NAME, 0);
    return ok;
}

static bool delete_state(int dirfd)
{
    struct stat st;
    if (!private_file_stat(dirfd, STATE_NAME, &st))
        return false;
    return unlinkat(dirfd, STATE_NAME, 0) == 0 && fsync(dirfd) == 0;
}

static bool can_read_or_ensure_state(const struct owned_state *state)
{
    if (getuid() == 0 || getuid() == state->uid)
        return true;
#ifndef UU_HELPER_TEST
    /* Executable group access authorizes adopting a root-created cold-boot record. */
    if (geteuid() == 0 && state->uid == 0)
        return true;
#else
    if (state->uid == 0)
        return true;
#endif
    return false;
}

static bool lock_state(int dirfd, bool create, int *lockfd_out)
{
    int flags = O_RDWR | O_CLOEXEC | O_NOFOLLOW;
    struct stat st;
    int fd;
    if (create)
        flags |= O_CREAT;
    fd = openat(dirfd, LOCK_NAME, flags, 0600);
    if (fd < 0)
        return false;
    if (fstat(fd, &st) != 0 || st.st_uid != trusted_uid() ||
        !S_ISREG(st.st_mode) || (st.st_mode & 077) != 0 || st.st_nlink != 1 ||
        flock(fd, LOCK_EX | LOCK_NB) != 0) {
        close(fd);
        return false;
    }
    *lockfd_out = fd;
    return true;
}

static int print_connector(const char *connector)
{
    char line[130];
    int n = snprintf(line, sizeof(line), "%s\n", connector);
    if (n < 0 || (size_t)n >= sizeof(line) || write(STDOUT_FILENO, line, (size_t)n) != n)
        return fail();
    return 0;
}

static int run_command(const char *command)
{
    int dirfd = -1;
    int lockfd = -1;
    bool has_state = false;
    struct owned_state state;
    bool connected = false, unknown = false;
    int result = 1;
    bool mutating = strcmp(command, "ensure") == 0 || strcmp(command, "release") == 0;

    if (strcmp(command, "ensure") != 0 && strcmp(command, "status") != 0 &&
        strcmp(command, "release") != 0)
        return fail();
    if (!open_state_dir(strcmp(command, "ensure") == 0, &dirfd)) {
        if (errno == ENOENT && strcmp(command, "status") == 0)
            return 0;
        return fail();
    }
    if (!lock_state(dirfd, strcmp(command, "ensure") == 0, &lockfd)) {
        struct stat st;
        if (fstatat(dirfd, STATE_NAME, &st, AT_SYMLINK_NOFOLLOW) != 0 &&
            errno == ENOENT && !mutating) {
            close(dirfd);
            return 0;
        }
        close(dirfd);
        return fail();
    }
    if (!read_state(dirfd, &state, &has_state))
        goto done;

    if (has_state) {
        char status[64];
        char force[64];
        if (!can_read_or_ensure_state(&state) ||
            !validate_owned_connector(state.connector, status, sizeof(status)))
            goto done;
        if (strcmp(command, "status") == 0) {
            result = print_connector(state.connector);
            goto done;
        }
        if (strcmp(command, "release") == 0) {
            if (getuid() != 0 && getuid() != state.uid)
                goto done;
            if (!clear_owned_force(dirfd, &state))
                goto done;
            result = 0;
            goto done;
        }
        if (!connector_force_state(state.connector, force, sizeof(force)))
            goto done;
        if (strcmp(status, "connected") == 0) {
            if (strcmp(force, "on") != 0 && strcmp(force, "unspecified") != 0)
                goto done;
            result = print_connector(state.connector);
            goto done;
        }
        if (strcmp(status, "disconnected") != 0 || strcmp(force, "unspecified") != 0 ||
            !connector_write(state.connector, "on\n"))
            goto done;
        if (!validate_owned_connector(state.connector, status, sizeof(status)) ||
            strcmp(status, "connected") != 0 ||
            !connector_force_state(state.connector, force, sizeof(force)) ||
            strcmp(force, "on") != 0)
            goto done;
        result = print_connector(state.connector);
        goto done;
    }

    if (strcmp(command, "status") == 0 || strcmp(command, "release") == 0) {
        result = 0;
        goto done;
    }

    if (!scan_connectors(&connected, &unknown, 0, false, NULL, NULL, 0) || unknown)
        goto done;
    if (connected) {
        result = 0;
        goto done;
    }
    {
        unsigned long card = 0, candidate_port = 0;
        unsigned count = 0;
        char candidate[128] = "";
        char status[64];
        char force[64];
        if (!find_i915_card(&card, &count) || count != 1)
            goto done;
        connected = false;
        unknown = false;
        if (!scan_connectors(&connected, &unknown, card, true, &candidate_port,
                             candidate, sizeof(candidate)) || unknown)
            goto done;
        if (connected) {
            result = 0;
            goto done;
        }
        if (!candidate[0] ||
            !validate_owned_connector(candidate, status, sizeof(status)) ||
            strcmp(status, "disconnected") != 0 ||
            !connector_force_state(candidate, force, sizeof(force)) ||
            strcmp(force, "unspecified") != 0)
            goto done;
        connected = false;
        unknown = false;
        if (!scan_connectors(&connected, &unknown, 0, false, NULL, NULL, 0) ||
            unknown)
            goto done;
        if (connected) {
            result = 0;
            goto done;
        }
        state.uid = getuid();
        strcpy(state.connector, candidate);
        if (!write_state(dirfd, &state))
            goto done;
        if (!connector_write(candidate, "on\n")) {
            (void)clear_owned_force(dirfd, &state);
            goto done;
        }
        if (!validate_owned_connector(candidate, status, sizeof(status)) ||
            strcmp(status, "connected") != 0 ||
            !connector_force_state(candidate, force, sizeof(force)) ||
            strcmp(force, "on") != 0) {
            (void)clear_owned_force(dirfd, &state);
            goto done;
        }
        result = print_connector(candidate);
    }
done:
    if (lockfd >= 0)
        close(lockfd);
    if (dirfd >= 0)
        close(dirfd);
    if (result != 0 && result != 1)
        return result;
    if (result == 0)
        return 0;
    return fail();
}

#ifdef UU_HELPER_TEST
static bool fixture_paths(const char *root_arg, const char *state_arg,
                          char *root_out, size_t root_cap,
                          char *state_out, size_t state_cap)
{
    char marker[PATH_MAX];
    char marker_contents[32];
    char root_parent[PATH_MAX];
    char state_parent[PATH_MAX];
    char canonical_parent[PATH_MAX];
    const char *root_slash;
    const char *state_slash;
    const char *state_base;
    struct stat st;
    size_t root_parent_len;

    if (!realpath(root_arg, root_out) || strlen(root_out) >= root_cap ||
        root_out[0] != '/' || strcmp(root_out, "/") == 0)
        return false;
    if (!path_join(marker, sizeof(marker), root_out,
                   ".uu-display-helper-fixture") ||
        lstat(marker, &st) != 0 || !S_ISREG(st.st_mode) ||
        st.st_uid != trusted_uid() || (st.st_mode & 0022) != 0 ||
        !read_small_file(marker, marker_contents, sizeof(marker_contents)) ||
        strcmp(marker_contents, "synthetic-v1") != 0)
        return false;

    root_slash = strrchr(root_out, '/');
    if (!root_slash || root_slash == root_out)
        return false;
    root_parent_len = (size_t)(root_slash - root_out);
    if (root_parent_len >= sizeof(root_parent))
        return false;
    memcpy(root_parent, root_out, root_parent_len);
    root_parent[root_parent_len] = '\0';

    if (!state_arg || state_arg[0] != '/' || strlen(state_arg) >= PATH_MAX)
        return false;
    state_slash = strrchr(state_arg, '/');
    if (!state_slash || !safe_component(state_slash + 1))
        return false;
    state_base = state_slash + 1;
    if (state_slash == state_arg) {
        strcpy(state_parent, "/");
    } else {
        size_t parent_len = (size_t)(state_slash - state_arg);
        if (parent_len >= sizeof(state_parent))
            return false;
        memcpy(state_parent, state_arg, parent_len);
        state_parent[parent_len] = '\0';
    }
    if (!realpath(state_parent, canonical_parent) ||
        strcmp(canonical_parent, root_parent) != 0 ||
        !path_join(state_out, state_cap, canonical_parent, state_base) ||
        strcmp(state_out, root_out) == 0)
        return false;
    return true;
}
#endif

int main(int argc, char **argv)
{
    const char *command = NULL;
#ifdef UU_HELPER_TEST
    if (argc == 5 && strcmp(argv[1], "--fixture") == 0) {
        char resolved_root[PATH_MAX];
        char resolved_state[PATH_MAX];
        if (!fixture_paths(argv[2], argv[3], resolved_root,
                           sizeof(resolved_root), resolved_state,
                           sizeof(resolved_state)))
            return fail();
        strcpy(sys_root, resolved_root);
        strcpy(state_dir, resolved_state);
        command = argv[4];
    } else {
        return fail();
    }
#else
    if (argc != 2 || geteuid() != 0)
        return fail();
    command = argv[1];
#endif
    if (!command)
        return fail();
    (void)signal(SIGALRM, timeout_handler);
    alarm(3);
    return run_command(command);
}
