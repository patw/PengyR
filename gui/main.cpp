#include <QApplication>
#include <QFont>
#include <QFontDatabase>
#include <QIcon>
#include <QJsonDocument>
#include <QJsonObject>
#include <cstdlib>
#include "pengy_ffi.h"
#include "mainwindow.h"

int main(int argc, char* argv[]) {
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
