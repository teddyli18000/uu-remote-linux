#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

#include <gtk/gtk.h>
#include <glib-unix.h>
#include <vte/vte.h>

#define MAX_OUTPUT_FRAME (16 * 1024)
#define MAX_VERIFY_TEXT 256
#define TEXT_CHECK_INTERVAL_MS 25
#define TEXT_CHECK_TIMEOUT_MS 3000
#define TEXT_CHECK_SETTLE_MS 500
#define TEXT_CHECK_STABLE_POLLS 8

static GtkWidget *window;
static GtkWidget *scroll;
static GtkWidget *fixed_grid;
static VteTerminal *terminal;
static GByteArray *pending;
static glong remote_cols = 80;
static glong remote_rows = 24;
static gboolean mapped_status_sent;

static guint32 get_u32_le(const guint8 *p) {
    return (guint32)p[0] | ((guint32)p[1] << 8) | ((guint32)p[2] << 16) | ((guint32)p[3] << 24);
}

static guint16 get_u16_le(const guint8 *p) {
    return (guint16)p[0] | ((guint16)p[1] << 8);
}

static void send_status(guint8 status) {
    ssize_t n;
    do { n = write(STDOUT_FILENO, &status, 1); } while (n < 0 && errno == EINTR);
}

/* The VTE grid is measured in cells; keep the widget at that exact pixel size inside the
 * scroll viewport so the top-level window can clip it without changing remote columns/rows. */
static void set_fixed_grid_size(void) {
    if (!gtk_widget_get_realized(GTK_WIDGET(terminal))) return;
    gtk_widget_set_size_request(GTK_WIDGET(terminal), -1, -1);
    gtk_widget_set_size_request(fixed_grid, -1, -1);
    vte_terminal_set_size(terminal, remote_cols > 0 ? remote_cols : 1, remote_rows > 0 ? remote_rows : 1);
    gint char_width = MAX(1, vte_terminal_get_char_width(terminal));
    gint char_height = MAX(1, vte_terminal_get_char_height(terminal));
    GtkStyleContext *style = gtk_widget_get_style_context(GTK_WIDGET(terminal));
    GtkStateFlags state = gtk_widget_get_state_flags(GTK_WIDGET(terminal));
    GtkBorder padding = {0};
    GtkBorder border = {0};
    gtk_style_context_get_padding(style, state, &padding);
    gtk_style_context_get_border(style, state, &border);
    gint width = (gint)remote_cols * char_width + padding.left + padding.right + border.left + border.right;
    gint height = (gint)remote_rows * char_height + padding.top + padding.bottom + border.top + border.bottom;
    gtk_widget_set_size_request(GTK_WIDGET(terminal), width, height);
    gtk_widget_set_size_request(fixed_grid, width, height);
    gtk_widget_queue_resize(GTK_WIDGET(terminal));
}

static void on_terminal_realize(GtkWidget *widget, gpointer data) {
    (void)widget;
    (void)data;
    set_fixed_grid_size();
}

typedef struct {
    glong cols;
    glong rows;
    gint width;
    gint height;
    gboolean window_shrank;
    gboolean kept_grid_when_small;
} GridCheck;

static gboolean finish_grid_check(gpointer data) {
    GridCheck *check = (GridCheck *)data;
    gboolean restored = vte_terminal_get_column_count(terminal) == check->cols &&
        vte_terminal_get_row_count(terminal) == check->rows;
    gint width, height;
    gtk_window_get_size(GTK_WINDOW(window), &width, &height);
    gboolean window_restored = width >= check->width - 2 && height >= check->height - 2;
    send_status(check->window_shrank && check->kept_grid_when_small && restored && window_restored ? 2 : 3);
    g_free(check);
    return G_SOURCE_REMOVE;
}

static gboolean check_small_window(gpointer data) {
    GridCheck *check = (GridCheck *)data;
    gint width, height;
    gtk_window_get_size(GTK_WINDOW(window), &width, &height);
    check->window_shrank = width < check->width || height < check->height;
    check->kept_grid_when_small = vte_terminal_get_column_count(terminal) == check->cols &&
        vte_terminal_get_row_count(terminal) == check->rows;
    gtk_window_resize(GTK_WINDOW(window), check->width, check->height);
    g_timeout_add(200, finish_grid_check, check);
    return G_SOURCE_REMOVE;
}

static gboolean begin_grid_check(gpointer unused) {
    (void)unused;
    GridCheck *check = g_new0(GridCheck, 1);
    check->cols = remote_cols;
    check->rows = remote_rows;
    gtk_window_get_size(GTK_WINDOW(window), &check->width, &check->height);
    if (check->width < 2 || check->height < 2) {
        check->width = 960;
        check->height = 600;
    }
    gtk_window_resize(GTK_WINDOW(window), MAX(240, check->width / 2), MAX(180, check->height / 2));
    g_timeout_add(200, check_small_window, check);
    return G_SOURCE_REMOVE;
}

typedef struct {
    char *needle;
    gboolean present;
    guint polls;
    guint stable_polls;
    gint64 started_at;
    char *last_text;
} TextCheck;

static gboolean finish_text_check(gpointer data) {
    TextCheck *check = (TextCheck *)data;
    char *text = vte_terminal_get_text(terminal, NULL, NULL, NULL);
    gboolean found = text != NULL && strstr(text, check->needle) != NULL;
    gboolean finished = FALSE;
    gboolean success = FALSE;
    check->polls++;
    if (check->present && found) {
        finished = TRUE;
        success = TRUE;
    } else if (!check->present && found) {
        finished = TRUE;
    } else if (!check->present) {
        const char *snapshot = text ? text : "";
        if (check->last_text && strcmp(check->last_text, snapshot) == 0) {
            check->stable_polls++;
        } else {
            check->stable_polls = 0;
        }
        g_free(check->last_text);
        check->last_text = g_strdup(snapshot);
        if (g_get_monotonic_time() - check->started_at >= TEXT_CHECK_SETTLE_MS * 1000 &&
            check->stable_polls >= TEXT_CHECK_STABLE_POLLS) {
            finished = TRUE;
            success = TRUE;
        }
    }
    if (!finished && check->polls * TEXT_CHECK_INTERVAL_MS >= TEXT_CHECK_TIMEOUT_MS) {
        finished = TRUE;
    }
    g_free(text);
    if (!finished) return G_SOURCE_CONTINUE;
    send_status(success ? 4 : 5);
    g_free(check->last_text);
    g_free(check->needle);
    g_free(check);
    return G_SOURCE_REMOVE;
}

static void parse_frames(void) {
    while (pending->len) {
        const guint8 *p = pending->data;
        if (p[0] == 1) {
            if (pending->len < 5) return;
            guint32 length = get_u32_le(p + 1);
            if (length > MAX_OUTPUT_FRAME) {
                g_warning("Terminal local view rejected an oversized output frame");
                g_byte_array_set_size(pending, 0);
                return;
            }
            if (pending->len < 5 + length) return;
            vte_terminal_feed(terminal, (const char *)p + 5, length);
            g_byte_array_remove_range(pending, 0, 5 + length);
        } else if (p[0] == 2) {
            if (pending->len < 5) return;
            remote_cols = MAX(1, get_u16_le(p + 1));
            remote_rows = MAX(1, get_u16_le(p + 3));
            set_fixed_grid_size();
            g_byte_array_remove_range(pending, 0, 5);
        } else if (p[0] == 3) {
            g_byte_array_remove_range(pending, 0, 1);
            g_idle_add(begin_grid_check, NULL);
        } else if (p[0] == 4) {
            if (pending->len < 6) return;
            guint32 length = get_u32_le(p + 2);
            if (length == 0 || length > MAX_VERIFY_TEXT) {
                g_warning("Terminal local view rejected an invalid text check");
                g_byte_array_remove_range(pending, 0, 6);
                continue;
            }
            if (pending->len < 6 + length) return;
            TextCheck *check = g_new0(TextCheck, 1);
            check->present = p[1] != 0;
            check->started_at = g_get_monotonic_time();
            check->needle = g_strndup((const char *)p + 6, length);
            g_byte_array_remove_range(pending, 0, 6 + length);
            g_timeout_add(TEXT_CHECK_INTERVAL_MS, finish_text_check, check);
        } else {
            g_warning("Terminal local view rejected an unknown frame");
            g_byte_array_remove_range(pending, 0, 1);
        }
    }
}

static gboolean read_output(gint fd, GIOCondition condition, gpointer user_data) {
    (void)user_data;
    guint8 buffer[65536];
    for (;;) {
        ssize_t n = read(fd, buffer, sizeof buffer);
        if (n > 0) {
            g_byte_array_append(pending, buffer, (guint)n);
            parse_frames();
        } else if (n == 0) {
            return G_SOURCE_REMOVE;
        } else if (errno == EINTR) {
            continue;
        } else if (errno == EAGAIN || errno == EWOULDBLOCK) {
            break;
        } else {
            return G_SOURCE_REMOVE;
        }
    }
    if (condition & (G_IO_HUP | G_IO_ERR | G_IO_NVAL)) return G_SOURCE_REMOVE;
    return G_SOURCE_CONTINUE;
}

static gboolean on_map(GtkWidget *widget, GdkEvent *event, gpointer data) {
    (void)widget;
    (void)event;
    (void)data;
    if (!mapped_status_sent) {
        mapped_status_sent = TRUE;
        send_status(1);
    }
    return FALSE;
}

static gboolean on_delete(GtkWidget *widget, GdkEvent *event, gpointer data) {
    (void)widget; (void)event; (void)data;
    gtk_main_quit();
    return FALSE;
}

void uu_terminal_view_run(uint32_t session_id, uint16_t cols, uint16_t rows) {
    if (!gtk_init_check(NULL, NULL)) {
        send_status(0);
        fprintf(stderr, "Terminal local view unavailable: GTK could not open a display\n");
        return;
    }
    remote_cols = cols ? cols : 80;
    remote_rows = rows ? rows : 24;
    window = gtk_window_new(GTK_WINDOW_TOPLEVEL);
    char title[96];
    g_snprintf(title, sizeof title, "UU Remote Terminal - session %u", session_id);
    gtk_window_set_title(GTK_WINDOW(window), title);
    gtk_window_set_default_size(GTK_WINDOW(window), 960, 600);
    g_signal_connect(window, "map-event", G_CALLBACK(on_map), NULL);
    g_signal_connect(window, "delete-event", G_CALLBACK(on_delete), NULL);

    scroll = gtk_scrolled_window_new(NULL, NULL);
    gtk_scrolled_window_set_policy(GTK_SCROLLED_WINDOW(scroll), GTK_POLICY_AUTOMATIC, GTK_POLICY_AUTOMATIC);
    gtk_widget_set_hexpand(scroll, TRUE);
    gtk_widget_set_vexpand(scroll, TRUE);
    gtk_container_add(GTK_CONTAINER(window), scroll);

    fixed_grid = gtk_fixed_new();
    gtk_widget_set_hexpand(fixed_grid, FALSE);
    gtk_widget_set_vexpand(fixed_grid, FALSE);
    gtk_container_add(GTK_CONTAINER(scroll), fixed_grid);

    terminal = VTE_TERMINAL(vte_terminal_new());
    vte_terminal_set_input_enabled(terminal, FALSE);
    vte_terminal_set_scrollback_lines(terminal, 10000);
    gtk_widget_set_hexpand(GTK_WIDGET(terminal), FALSE);
    gtk_widget_set_vexpand(GTK_WIDGET(terminal), FALSE);
    gtk_fixed_put(GTK_FIXED(fixed_grid), GTK_WIDGET(terminal), 0, 0);
    g_signal_connect(terminal, "realize", G_CALLBACK(on_terminal_realize), NULL);
    gtk_widget_show_all(window);
    gtk_window_present(GTK_WINDOW(window));

    int flags = fcntl(STDIN_FILENO, F_GETFL, 0);
    if (flags >= 0) fcntl(STDIN_FILENO, F_SETFL, flags | O_NONBLOCK);
    pending = g_byte_array_new();
    g_unix_fd_add(STDIN_FILENO, G_IO_IN | G_IO_HUP | G_IO_ERR | G_IO_NVAL, read_output, NULL);
    gtk_main();
    g_byte_array_unref(pending);
    gtk_widget_destroy(window);
}
