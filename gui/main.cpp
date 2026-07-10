#include <QApplication>
#include <QFont>
#include <QFontDatabase>
#include <QIcon>
#include <QJsonDocument>
#include <QJsonObject>
#include <QTextStream>
#include <cstdlib>
#include "pengy_ffi.h"
#include "mainwindow.h"
#include "version.h"

static void showHelp(const char* argv0) {
    QTextStream out(stdout);
    out << "Pengy v" << PENGY_VERSION << " — Local-first AI agent with tools (GUI, Rust core)\n\n";
    out << "Usage: " << (argv0 ? argv0 : "pengy") << " [OPTIONS]\n\n";
    out << "Options:\n";
    out << "  -h, --help     Show this help message and exit.\n";
    out << "  -v, --version  Show version information and exit.\n\n";
    out << "The desktop GUI launches a Qt6 window. No additional\n";
    out << "command-line options are supported.\n";
}

int main(int argc, char* argv[]) {
    // Handle -v/--version and -h/--help before creating QApplication
    for (int i = 1; i < argc; ++i) {
        const QString arg = QString::fromUtf8(argv[i]);
        if (arg == "-v" || arg == "--version") {
            QTextStream(stdout) << "Pengy v" << PENGY_VERSION << "\n";
            return 0;
        }
        if (arg == "-h" || arg == "--help") {
            showHelp(argv[0]);
            return 0;
        }
    }

    // Load config to get UI scale, then set QT_SCALE_FACTOR before QApplication
    char* cfgJson = pengy_config_load();
    QJsonObject cfg = QJsonDocument::fromJson(QByteArray(cfgJson)).object();
    pengy_free(cfgJson);

    int scale = cfg.value("ui_scale").toInt(100);
    if (scale != 100) {
        qputenv("QT_SCALE_FACTOR", QByteArray::number(scale / 100.0, 'f', 2));
    }

    QApplication app(argc, argv);
    app.setApplicationName("Pengy");
    app.setOrganizationName("Pengy");
    app.setWindowIcon(QIcon(":/pengy.png"));

    QFont font = QFontDatabase::systemFont(QFontDatabase::GeneralFont);
    font.setPointSize(10);
    app.setFont(font);

    MainWindow window;
    window.show();
    return app.exec();
}
