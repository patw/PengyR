#include "settingsdialog.h"
#include "pengy_ffi.h"
#include <QVBoxLayout>
#include <QFormLayout>
#include <QDialogButtonBox>
#include <QHBoxLayout>
#include <QLabel>
#include <QMessageBox>
#include <QThread>
#include <QJsonDocument>
#include <QJsonArray>
#include <QNetworkAccessManager>
#include <QNetworkRequest>
#include <QNetworkReply>
#include <QEventLoop>
#include <QPointer>

SettingsDialog::SettingsDialog(QJsonObject config, QWidget* parent)
    : QDialog(parent), m_config(config) {
    setWindowTitle("Settings");
    setModal(true);
    resize(520, 520);

    auto* layout = new QVBoxLayout(this);

    auto* form = new QFormLayout;
    form->setFieldGrowthPolicy(QFormLayout::ExpandingFieldsGrow);

    m_baseUrl = new QLineEdit(config["base_url"].toString("https://api.openai.com/v1"));
    m_apiKey = new QLineEdit(config["api_key"].toString());
    m_apiKey->setEchoMode(QLineEdit::Password);

    // Model: editable combo + fetch button (matching Python layout)
    auto* modelRow = new QHBoxLayout;
    m_model = new QComboBox;
    m_model->setEditable(true);
    m_model->setInsertPolicy(QComboBox::NoInsert);
    QString currentModel = config["model"].toString("gpt-4o");
    m_model->addItem(currentModel);
    m_model->setCurrentText(currentModel);
    modelRow->addWidget(m_model, 1);

    m_fetchBtn = new QPushButton("↻ Fetch");
    m_fetchBtn->setToolTip("Fetch available models from the endpoint");
    m_fetchBtn->setFixedWidth(80);
    connect(m_fetchBtn, &QPushButton::clicked, this, &SettingsDialog::fetchModels);
    modelRow->addWidget(m_fetchBtn);

    m_userAgent = new QLineEdit(config["user_agent"].toString("PengyAgent/1.0"));

    form->addRow("Base URL:", m_baseUrl);
    form->addRow("API Key:", m_apiKey);
    form->addRow("Model:", modelRow);
    form->addRow("User Agent:", m_userAgent);

    m_systemMsg = new QTextEdit(config["system_message"].toString("You are a helpful assistant."));
    m_systemMsg->setMaximumHeight(80);
    form->addRow("System Message:", m_systemMsg);

    m_toolConfirm = new QComboBox;
    m_toolConfirm->addItem("YOLO (All) — execute everything, no questions asked", "all");
    m_toolConfirm->addItem("Safe Only — auto-approve read-only tools; confirm write/execute", "safe");
    m_toolConfirm->addItem("None — confirm every tool before execution", "none");
    QString tc = config["tool_confirmation"].toString("none");
    for (int i = 0; i < m_toolConfirm->count(); i++) {
        if (m_toolConfirm->itemData(i).toString() == tc) {
            m_toolConfirm->setCurrentIndex(i);
            break;
        }
    }
    form->addRow("Tool Confirmation:", m_toolConfirm);

    m_contextKeep = new QSpinBox;
    m_contextKeep->setRange(0, 999);
    m_contextKeep->setSpecialValueText("Keep all");
    m_contextKeep->setSuffix(" turns");
    m_contextKeep->setToolTip("Tool results older than this many turns are elided to save context window. 0 = keep all.");
    m_contextKeep->setValue(config["context_keep_turns"].toInt(0));
    form->addRow("Keep tool results:", m_contextKeep);

    m_uiScale = new QComboBox;
    int scales[] = {75, 100, 125, 150, 175, 200};
    int currentScale = config["ui_scale"].toInt(100);
    int idx = 1;
    for (int i = 0; i < 6; i++) {
        m_uiScale->addItem(QString("%1%").arg(scales[i]), scales[i]);
        if (scales[i] == currentScale) idx = i;
    }
    m_uiScale->setCurrentIndex(idx);
    form->addRow("UI Scale (restart to apply):", m_uiScale);

    m_toolTimeout = new QSpinBox;
    m_toolTimeout->setRange(-1, 3600);
    m_toolTimeout->setSpecialValueText("No timeout");
    m_toolTimeout->setSuffix(" sec");
    m_toolTimeout->setValue(config["tool_timeout"].toInt(60));
    form->addRow("Tool timeout:", m_toolTimeout);

    layout->addLayout(form);
    layout->addStretch();

    auto* buttons = new QDialogButtonBox(QDialogButtonBox::Ok | QDialogButtonBox::Cancel);
    connect(buttons, &QDialogButtonBox::accepted, this, [this]() {
        m_config["base_url"] = m_baseUrl->text();
        m_config["api_key"] = m_apiKey->text();
        m_config["model"] = m_model->currentText();
        m_config["user_agent"] = m_userAgent->text();
        m_config["system_message"] = m_systemMsg->toPlainText();
        m_config["tool_confirmation"] = m_toolConfirm->currentData().toString();
        m_config["context_keep_turns"] = m_contextKeep->value();
        m_config["ui_scale"] = m_uiScale->currentData().toInt();
        m_config["tool_timeout"] = m_toolTimeout->value();
        accept();
    });
    connect(buttons, &QDialogButtonBox::rejected, this, &QDialog::reject);
    layout->addWidget(buttons);
}

void SettingsDialog::fetchModels() {
    m_fetchBtn->setEnabled(false);
    m_fetchBtn->setText("...");

    QString baseUrl = m_baseUrl->text().trimmed();
    QString apiKey = m_apiKey->text();

    // Run in a background thread to avoid blocking the UI
    QThread* thread = QThread::create([this, baseUrl, apiKey]() {
        QNetworkAccessManager mgr;
        QNetworkRequest req(QUrl(baseUrl + "/models"));
        req.setRawHeader("Authorization", ("Bearer " + apiKey).toUtf8());
        req.setRawHeader("api-key", apiKey.toUtf8());
        req.setRawHeader("User-Agent", "PengyAgent/1.0");

        QNetworkReply* reply = mgr.get(req);
        QEventLoop loop;
        QObject::connect(reply, &QNetworkReply::finished, &loop, &QEventLoop::quit);
        loop.exec();

        // Capture what we need to pass back to UI thread
        QComboBox* model = m_model;
        QPushButton* btn = m_fetchBtn;
        QPointer<SettingsDialog> self = this;

        if (reply->error() == QNetworkReply::NoError) {
            QByteArray data = reply->readAll();
            QJsonDocument doc = QJsonDocument::fromJson(data);
            QJsonArray arr = doc.object()["data"].toArray();
            QStringList modelIds;
            for (const QJsonValue& v : arr) {
                QString id = v.toObject()["id"].toString();
                if (!id.isEmpty()) modelIds << id;
            }
            modelIds.sort();

            QMetaObject::invokeMethod(model, [model, btn, modelIds, self]() {
                btn->setEnabled(true);
                btn->setText("↻ Fetch");
                if (modelIds.isEmpty()) {
                    if (self) QMessageBox::information(self, "No Models", "The endpoint returned an empty model list.");
                    return;
                }
                QString current = model->currentText();
                model->clear();
                model->addItems(modelIds);
                if (modelIds.contains(current)) {
                    model->setCurrentText(current);
                } else {
                    model->setCurrentText(modelIds.first());
                }
            }, Qt::QueuedConnection);
        } else {
            int code = reply->attribute(QNetworkRequest::HttpStatusCodeAttribute).toInt();
            QString err = QString("HTTP %1 from %2/models\n\nCheck your Base URL and API Key.")
                .arg(code).arg(baseUrl);
            QMetaObject::invokeMethod(model, [btn, err, self]() {
                btn->setEnabled(true);
                btn->setText("↻ Fetch");
                if (self) QMessageBox::warning(self, "Fetch Failed", err);
            }, Qt::QueuedConnection);
        }
        reply->deleteLater();
    });
    connect(thread, &QThread::finished, thread, &QObject::deleteLater);
    thread->start();
}
